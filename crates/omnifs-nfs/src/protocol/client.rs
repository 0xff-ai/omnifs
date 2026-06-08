use crate::export::{Status, StatusResult};
use crate::protocol::filehandle::client_id_for_slot;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub(crate) struct ClientTable {
    generation: u64,
    next_slot: AtomicU64,
    records: Mutex<HashMap<u64, ClientRecord>>,
}

#[derive(Debug, Clone)]
struct ClientRecord {
    verifier: [u8; 8],
    owner: Vec<u8>,
    confirm: [u8; 8],
    confirmed: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientAssignment {
    pub(crate) clientid: u64,
    pub(crate) confirm: [u8; 8],
}

impl ClientTable {
    pub(crate) fn new(generation: u64) -> Self {
        Self {
            generation,
            next_slot: AtomicU64::new(1),
            records: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn set_clientid(&self, verifier: [u8; 8], owner: Vec<u8>) -> ClientAssignment {
        let mut records = self.records.lock().expect("NFS client table lock");
        if let Some((clientid, record)) = records
            .iter()
            .find(|(_, record)| record.verifier == verifier && record.owner == owner)
        {
            return ClientAssignment {
                clientid: *clientid,
                confirm: record.confirm,
            };
        }

        let slot = self.next_slot.fetch_add(1, Ordering::Relaxed);
        let clientid = client_id_for_slot(self.generation, slot);
        let confirm = confirm_verifier(self.generation, clientid, verifier, &owner);
        records.insert(
            clientid,
            ClientRecord {
                verifier,
                owner,
                confirm,
                confirmed: false,
            },
        );
        ClientAssignment { clientid, confirm }
    }

    pub(crate) fn confirm(&self, clientid: u64, verifier: &[u8]) -> StatusResult<()> {
        let mut records = self.records.lock().expect("NFS client table lock");
        let Some(record) = records.get_mut(&clientid) else {
            return Err(Status::StaleClientId);
        };
        if verifier != record.confirm {
            return Err(Status::StaleClientId);
        }
        record.confirmed = true;
        Ok(())
    }

    pub(crate) fn is_confirmed(&self, clientid: u64) -> bool {
        self.records
            .lock()
            .expect("NFS client table lock")
            .get(&clientid)
            .is_some_and(|record| record.confirmed)
    }

    #[cfg(test)]
    pub(crate) fn with_confirmed_default(generation: u64) -> Self {
        let table = Self::new(generation);
        let verifier = [0; 8];
        let owner = b"test-client".to_vec();
        let clientid = crate::protocol::filehandle::client_id(generation);
        let confirm = confirm_verifier(generation, clientid, verifier, &owner);
        table.records.lock().expect("NFS client table lock").insert(
            clientid,
            ClientRecord {
                verifier,
                owner,
                confirm,
                confirmed: true,
            },
        );
        table
    }
}

fn confirm_verifier(generation: u64, clientid: u64, verifier: [u8; 8], owner: &[u8]) -> [u8; 8] {
    let mut hasher = DefaultHasher::new();
    generation.hash(&mut hasher);
    clientid.hash(&mut hasher);
    verifier.hash(&mut hasher);
    owner.hash(&mut hasher);
    hasher.finish().to_be_bytes()
}
