use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use common::key::Key;
use common::value::Value;
use shared::newtypes::Blake2bHash;
use shared::transform::Transform;

use error;
use global_state::StateReader;
use global_state::{commit, CommitResult, History};
use trie::operations::create_hashed_empty_trie;
use trie::Trie;
use trie_store::in_memory::{
    self, InMemoryEnvironment, InMemoryReadTransaction, InMemoryTrieStore,
};
use trie_store::operations::{read, write, ReadResult, WriteResult};
use trie_store::{Transaction, TransactionSource, TrieStore};

/// Represents a "view" of global state at a particular root hash.
pub struct InMemoryGlobalState {
    pub environment: Arc<InMemoryEnvironment>,
    pub store: Arc<InMemoryTrieStore>,
    pub root_hash: Blake2bHash,
}

impl InMemoryGlobalState {
    /// Creates an empty state.
    pub fn empty() -> Result<Self, error::Error> {
        let environment = Arc::new(InMemoryEnvironment::new());
        let store = Arc::new(InMemoryTrieStore::new(&environment));
        let root_hash: Blake2bHash = {
            let (root_hash, root) = create_hashed_empty_trie::<Key, Value>()?;
            let mut txn = environment.create_read_write_txn()?;
            store.put(&mut txn, &root_hash, &root)?;
            txn.commit()?;
            root_hash
        };
        Ok(InMemoryGlobalState::new(environment, store, root_hash))
    }

    /// Creates a state from an existing environment, store, and root_hash.
    /// Intended to be used for testing.
    pub(crate) fn new(
        environment: Arc<InMemoryEnvironment>,
        store: Arc<InMemoryTrieStore>,
        root_hash: Blake2bHash,
    ) -> Self {
        InMemoryGlobalState {
            environment,
            store,
            root_hash,
        }
    }

    /// Creates a state from a given set of [`Key`](common::key::key), [`Value`](common::value::Value) pairs
    pub fn from_pairs(pairs: &[(Key, Value)]) -> Result<Self, error::Error> {
        let mut ret = InMemoryGlobalState::empty()?;
        {
            let mut txn = ret.environment.create_read_write_txn()?;
            let mut current_root = ret.root_hash;
            for (key, value) in pairs {
                match write::<_, _, _, InMemoryTrieStore, in_memory::Error>(
                    &mut txn,
                    &ret.store,
                    &current_root,
                    key,
                    value,
                )? {
                    WriteResult::Written(root_hash) => {
                        current_root = root_hash;
                    }
                    WriteResult::AlreadyExists => (),
                    WriteResult::RootNotFound => panic!("InMemoryGlobalState has invalid root"),
                }
            }
            ret.root_hash = current_root;
            txn.commit()?;
        }
        Ok(ret)
    }
}

impl StateReader<Key, Value> for InMemoryGlobalState {
    type Error = error::Error;

    fn read(&self, key: &Key) -> Result<Option<Value>, Self::Error> {
        let txn = self.environment.create_read_txn()?;
        let ret = match read::<Key, Value, InMemoryReadTransaction, InMemoryTrieStore, Self::Error>(
            &txn,
            self.store.deref(),
            &self.root_hash,
            key,
        )? {
            ReadResult::Found(value) => Some(value),
            ReadResult::NotFound => None,
            ReadResult::RootNotFound => panic!("InMemoryGlobalState has invalid root"),
        };
        txn.commit()?;
        Ok(ret)
    }
}

impl History for InMemoryGlobalState {
    type Error = error::Error;

    type Reader = Self;

    fn checkout(&self, prestate_hash: Blake2bHash) -> Result<Option<Self::Reader>, Self::Error> {
        let txn = self.environment.create_read_txn()?;
        let maybe_root: Option<Trie<Key, Value>> = self.store.get(&txn, &prestate_hash)?;
        let maybe_state = maybe_root.map(|_| InMemoryGlobalState {
            environment: Arc::clone(&self.environment),
            store: Arc::clone(&self.store),
            root_hash: prestate_hash,
        });
        txn.commit()?;
        Ok(maybe_state)
    }

    fn commit(
        &mut self,
        prestate_hash: Blake2bHash,
        effects: HashMap<Key, Transform>,
    ) -> Result<CommitResult, Self::Error> {
        let commit_result = commit::<InMemoryEnvironment, InMemoryTrieStore, _, Self::Error>(
            &self.environment,
            &self.store,
            prestate_hash,
            effects,
        )?;
        if let CommitResult::Success(root_hash) = commit_result {
            self.root_hash = root_hash;
        };
        Ok(commit_result)
    }
}

#[cfg(test)]
mod tests {
    use shared::init::mocked_account;

    use super::*;

    #[derive(Debug, Clone)]
    struct TestPair {
        key: Key,
        value: Value,
    }

    const TEST_PAIRS: [TestPair; 2] = [
        TestPair {
            key: Key::Account([1u8; 32]),
            value: Value::Int32(1),
        },
        TestPair {
            key: Key::Account([2u8; 32]),
            value: Value::Int32(2),
        },
    ];

    fn create_test_pairs_updated() -> [TestPair; 3] {
        [
            TestPair {
                key: Key::Account([1u8; 32]),
                value: Value::String("one".to_string()),
            },
            TestPair {
                key: Key::Account([2u8; 32]),
                value: Value::String("two".to_string()),
            },
            TestPair {
                key: Key::Account([3u8; 32]),
                value: Value::Int32(3),
            },
        ]
    }

    fn create_test_state() -> InMemoryGlobalState {
        InMemoryGlobalState::from_pairs(
            &TEST_PAIRS
                .iter()
                .cloned()
                .map(|TestPair { key, value }| (key, value))
                .collect::<Vec<(Key, Value)>>(),
        )
        .unwrap()
    }

    #[test]
    fn reads_from_a_checkout_return_expected_values() {
        let state = create_test_state();
        let checkout = state.checkout(state.root_hash).unwrap().unwrap();
        for TestPair { key, value } in TEST_PAIRS.iter().cloned() {
            assert_eq!(Some(value), checkout.read(&key).unwrap());
        }
    }

    #[test]
    fn checkout_fails_if_unknown_hash_is_given() {
        let state = create_test_state();
        let fake_hash: Blake2bHash = [1u8; 32].into();
        let result = state.checkout(fake_hash).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn commit_updates_state() {
        let test_pairs_updated = create_test_pairs_updated();

        let mut state = create_test_state();
        let root_hash = state.root_hash;

        let effects: HashMap<Key, Transform> = test_pairs_updated
            .iter()
            .cloned()
            .map(|TestPair { key, value }| (key, Transform::Write(value)))
            .collect();

        let updated_hash = match state.commit(root_hash, effects).unwrap() {
            CommitResult::Success(hash) => hash,
            _ => panic!("commit failed"),
        };

        let updated_checkout = state.checkout(updated_hash).unwrap().unwrap();

        for TestPair { key, value } in test_pairs_updated.iter().cloned() {
            assert_eq!(Some(value), updated_checkout.read(&key).unwrap());
        }
    }

    #[test]
    fn commit_updates_state_and_original_state_stays_intact() {
        let test_pairs_updated = create_test_pairs_updated();

        let mut state = create_test_state();
        let root_hash = state.root_hash;

        let effects: HashMap<Key, Transform> = {
            let mut tmp = HashMap::new();
            for TestPair { key, value } in &test_pairs_updated {
                tmp.insert(*key, Transform::Write(value.to_owned()));
            }
            tmp
        };

        let updated_hash = match state.commit(root_hash, effects).unwrap() {
            CommitResult::Success(hash) => hash,
            _ => panic!("commit failed"),
        };

        let updated_checkout = state.checkout(updated_hash).unwrap().unwrap();
        for TestPair { key, value } in test_pairs_updated.iter().cloned() {
            assert_eq!(Some(value), updated_checkout.read(&key).unwrap());
        }

        let original_checkout = state.checkout(root_hash).unwrap().unwrap();
        for TestPair { key, value } in TEST_PAIRS.iter().cloned() {
            assert_eq!(Some(value), original_checkout.read(&key).unwrap());
        }
        assert_eq!(
            None,
            original_checkout.read(&test_pairs_updated[2].key).unwrap()
        );
    }

    #[test]
    fn initial_state_has_the_expected_hash() {
        let expected_bytes = vec![
            193u8, 151, 167, 126, 241, 141, 9, 163, 43, 169, 238, 19, 93, 87, 183, 131, 99, 160,
            96, 216, 8, 39, 227, 218, 246, 90, 65, 57, 207, 242, 61, 205,
        ];
        let init_state = mocked_account([48u8; 32]);
        let global_state = InMemoryGlobalState::from_pairs(&init_state).unwrap();
        assert_eq!(expected_bytes, global_state.root_hash.to_vec())
    }
}
