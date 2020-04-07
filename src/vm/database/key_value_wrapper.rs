use super::{MarfedKV, ClarityBackingStore, ClarityDeserializable};
use vm::Value;
use vm::errors::{ InterpreterResult as Result };
use chainstate::{ burn::BlockHeaderHash, stacks::index::proofs::TrieMerkleProof };
use std::collections::{HashMap};
use util::hash::{Sha512Trunc256Sum};
use vm::types::{QualifiedContractIdentifier, TypeSignature};
use std::{cmp::Eq, hash::Hash, clone::Clone};

#[cfg(rollback_value_check)]
type RollbackValueCheck = String;
#[cfg(not(rollback_value_check))]
type RollbackValueCheck = ();

#[cfg(not(rollback_value_check))]
fn rollback_value_check(_value: &String, _check: &RollbackValueCheck) {}

#[cfg(not(rollback_value_check))]
fn rollback_edits_push<T>(edits: &mut Vec<(T, RollbackValueCheck)>, key: T, _value: &String) {
    edits.push((key, ()));
}
// this function is used to check the lookup map when committing at the "bottom" of the
//   wrapper -- i.e., when committing to the underlying store. for the _unchecked_ implementation
//   this is used to get the edit _value_ out of the lookupmap, for used in the subsequent `put_all`
//   command.
#[cfg(not(rollback_value_check))]
fn rollback_check_pre_bottom_commit<T>(edits: Vec<(T, RollbackValueCheck)>, lookup_map: &mut HashMap<T, Vec<String>>) -> Vec<(T, String)>
where T: Eq + Hash + Clone {
    for (_, edit_history) in lookup_map.iter_mut() {
        edit_history.reverse();
    }

    let output = edits.into_iter().map(|(key, _)| {
        let value = rollback_lookup_map(&key, &(), lookup_map);
        (key, value)
    }).collect();

    assert!(lookup_map.len() == 0);
    output
}

#[cfg(rollback_value_check)]
fn rollback_value_check(value: &String, check: &RollbackValueCheck) {
    assert_eq!(value, check)
}
#[cfg(rollback_value_check)]
fn rollback_edits_push<T>(edits: &mut Vec<(T, RollbackValueCheck)>, key: T, value: &String)
where T: Eq + Hash + Clone {
    edits.push((key, value.clone()));
}
// this function is used to check the lookup map when committing at the "bottom" of the
//   wrapper -- i.e., when committing to the underlying store.
#[cfg(rollback_value_check)]
fn rollback_check_pre_bottom_commit<T>(edits: Vec<(T, RollbackValueCheck)>, lookup_map: &mut HashMap<T, Vec<String>>) -> Vec<(T, String)>
where T: Eq + Hash + Clone {
    for (_, edit_history) in lookup_map.iter_mut() {
        edit_history.reverse();
    }
    for (key, value) in edits.iter() {
        rollback_lookup_map(key, &value, lookup_map);
    }
    assert!(lookup_map.len() == 0);
    edits
}

pub struct RollbackContext {
    edits: Vec<(String, RollbackValueCheck)>,
    metadata_edits: Vec<((QualifiedContractIdentifier, String), RollbackValueCheck)>,
}

pub struct RollbackWrapper <'a> {
    // the underlying key-value storage.
    store: &'a mut dyn ClarityBackingStore,
    // lookup_map is a history of edits for a given key.
    //   in order of least-recent to most-recent at the tail.
    //   this allows ~ O(1) lookups, and ~ O(1) commits, roll-backs (amortized by # of PUTs).
    lookup_map: HashMap<String, Vec<String>>,
    metadata_lookup_map: HashMap<(QualifiedContractIdentifier, String), Vec<String>>,
    // stack keeps track of the most recent rollback context, which tells us which
    //   edits were performed by which context. at the moment, each context's edit history
    //   is a separate Vec which must be drained into the parent on commits, meaning that
    //   the amortized cost of committing a value isn't O(1), but actually O(k) where k is
    //   stack depth.
    //  TODO: The solution to this is to just have a _single_ edit stack, and merely store indexes
    //   to indicate a given contexts "start depth".
    stack: Vec<RollbackContext>
}

fn rollback_lookup_map<T>(key: &T, value: &RollbackValueCheck, lookup_map: &mut HashMap<T, Vec<String>>) -> String
where T: Eq + Hash + Clone {
    let popped_value;
    let remove_edit_deque = {
        let key_edit_history = lookup_map.get_mut(key)
            .expect("ERROR: Clarity VM had edit log entry, but not lookup_map entry");
        popped_value = key_edit_history.pop().unwrap();
        rollback_value_check(&popped_value, value);
        key_edit_history.len() == 0
    };
    if remove_edit_deque {
        lookup_map.remove(key);
    }
    popped_value
}

impl <'a> RollbackWrapper <'a> {
    pub fn new(store: &'a mut dyn ClarityBackingStore) -> RollbackWrapper {
        RollbackWrapper {
            store: store,
            lookup_map: HashMap::new(),
            metadata_lookup_map: HashMap::new(),
            stack: Vec::new()
        }
    }

    pub fn nest(&mut self) {
        self.stack.push(RollbackContext { edits: Vec::new(),
                                          metadata_edits: Vec::new() });
    }

    // Rollback the child's edits.
    //   this clears all edits from the child's edit queue,
    //     and removes any of those edits from the lookup map.
    pub fn rollback(&mut self) {
        let mut last_item = self.stack.pop()
            .expect("ERROR: Clarity VM attempted to commit past the stack.");

        last_item.edits.reverse();
        last_item.metadata_edits.reverse();

        for (key, value) in last_item.edits.drain(..) {
            rollback_lookup_map(&key, &value, &mut self.lookup_map);
        }

        for (key, value) in last_item.metadata_edits.drain(..) {
            rollback_lookup_map(&key, &value, &mut self.metadata_lookup_map);
        }
    }

    pub fn commit(&mut self) {
        let mut last_item = self.stack.pop()
            .expect("ERROR: Clarity VM attempted to commit past the stack.");

        if self.stack.len() == 0 {
            // committing to the backing store
            let all_edits = rollback_check_pre_bottom_commit(
                last_item.edits, &mut self.lookup_map);
            if all_edits.len() > 0 {
                self.store.put_all(all_edits);
            }

            let metadata_edits = rollback_check_pre_bottom_commit(
                last_item.metadata_edits, &mut self.metadata_lookup_map);
            if metadata_edits.len() > 0 {
                self.store.put_all_metadata(metadata_edits);
            }
        } else {
            // bubble up to the next item in the stack
            let next_up = self.stack.last_mut().unwrap();
            for (key, value) in last_item.edits.drain(..) {
                next_up.edits.push((key, value));
            }
            for (key, value) in last_item.metadata_edits.drain(..) {
                next_up.metadata_edits.push((key, value));
            }
        }
    }
}

fn inner_put<T>(lookup_map: &mut HashMap<T, Vec<String>>, edits: &mut Vec<(T, RollbackValueCheck)>, key: T, value: String)
where T: Eq + Hash + Clone {
    if !lookup_map.contains_key(&key) {
        lookup_map.insert(key.clone(), Vec::new());
    }
    let key_edit_deque = lookup_map.get_mut(&key).unwrap();
    rollback_edits_push(edits, key, &value);
    key_edit_deque.push(value);
}

impl <'a> RollbackWrapper <'a> {
    pub fn put(&mut self, key: &str, value: &str) {
        let current = self.stack.last_mut()
            .expect("ERROR: Clarity VM attempted PUT on non-nested context.");

        inner_put(&mut self.lookup_map, &mut current.edits, key.to_string(), value.to_string())
    }

    pub fn set_block_hash(&mut self, bhh: BlockHeaderHash) -> Result<BlockHeaderHash> {
        self.store.set_block_hash(bhh)
    }

    /// this function will only return commitment proofs for values _already_ materialized
    ///  in the underlying store. otherwise it returns None.
    pub fn get_with_proof<T>(&mut self, key: &str) -> Option<(T, TrieMerkleProof)> where T: ClarityDeserializable<T> {
        self.store.get_with_proof(key)
            .map(|(value, proof)| (T::deserialize(&value), proof))
    }

    pub fn get<T>(&mut self, key: &str) -> Option<T> where T: ClarityDeserializable<T> {
        self.stack.last()
            .expect("ERROR: Clarity VM attempted GET on non-nested context.");

        let lookup_result = self.lookup_map.get(key)
            .and_then(|x| x.last())
            .map(|x| T::deserialize(x));

        lookup_result
            .or_else(|| self.store.get(key).map(|x| T::deserialize(&x)))
    }

    pub fn get_value(&mut self, key: &str, expected: &TypeSignature) -> Option<Value> {
        self.stack.last()
            .expect("ERROR: Clarity VM attempted GET on non-nested context.");

        let lookup_result = self.lookup_map.get(key)
            .and_then(|x| x.last())
            .map(|x| Value::deserialize(x, expected));

        lookup_result
            .or_else(|| self.store.get(key).map(|x| Value::deserialize(&x, expected)))
    }

    pub fn get_current_block_height(&mut self) -> u32 {
        self.store.get_current_block_height()
    }

    pub fn get_block_header_hash(&mut self, block_height: u32) -> Option<BlockHeaderHash> {
        self.store.get_block_at_height(block_height)
    }

    pub fn prepare_for_contract_metadata(&mut self, contract: &QualifiedContractIdentifier, content_hash: Sha512Trunc256Sum) {
        let key = MarfedKV::make_contract_hash_key(contract);
        let value = self.store.make_contract_commitment(content_hash);
        self.put(&key, &value)
    }

    pub fn insert_metadata(&mut self, contract: &QualifiedContractIdentifier, key: &str, value: &str) {
        let current = self.stack.last_mut()
            .expect("ERROR: Clarity VM attempted PUT on non-nested context.");

        let metadata_key = (contract.clone(), key.to_string());

        inner_put(&mut self.metadata_lookup_map, &mut current.metadata_edits, metadata_key, value.to_string())
    }

    // Throws a NoSuchContract error if contract doesn't exist,
    //   returns None if there is no such metadata field.
    pub fn get_metadata(&mut self, contract: &QualifiedContractIdentifier, key: &str) -> Result<Option<String>> {
        self.stack.last()
            .expect("ERROR: Clarity VM attempted GET on non-nested context.");

        // This is THEORETICALLY a spurious clone, but it's hard to turn something like
        //  (&A, &B) into &(A, B).
        let metadata_key = (contract.clone(), key.to_string());
        let lookup_result = self.metadata_lookup_map.get(&metadata_key)
            .and_then(|x| x.last().cloned());

        match lookup_result {
            Some(x) => Ok(Some(x)),
            None => {
                self.store.get_metadata(contract, key)
            }
        }
    }

    pub fn has_entry(&mut self, key: &str) -> bool {
        self.stack.last()
            .expect("ERROR: Clarity VM attempted GET on non-nested context.");
        if self.lookup_map.contains_key(key) {
            true
        } else {
            self.store.has_entry(key)
        }
    }

    pub fn has_metadata_entry(&mut self, contract: &QualifiedContractIdentifier, key: &str) -> bool {
        match self.get_metadata(contract, key) {
            Ok(Some(_)) => true,
            _ => false
        }
    }
}
