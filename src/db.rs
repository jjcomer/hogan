use failure::Error;
use rocksdb::{DBIterator, DBVector, DB};
use serde_json::{self, Value};
use tempfile::tempdir;
use tempfile::TempDir;

pub struct ConfigDB {
    db: DB,
    tempdir: TempDir,
}

impl ConfigDB {
    pub fn new() -> Result<ConfigDB, Error> {
        let td = tempdir()?;
        let path = td.path().join("hogan_db");

        info!("Creating db: {:?}", path);
        let db = DB::open_default(path)?;
        Ok(ConfigDB { db, tempdir: td })
    }

    pub fn get(&self, key: &str) -> Option<DBVector> {
        match self.db.get(key) {
            Ok(result) => result,
            Err(e) => {
                error!("Unable to access db {:?}", e);
                None
            }
        }
    }

    pub fn scan(&self, prefix: &str) -> DBIterator {
        debug!("Scanning for {}", prefix);
        self.db.prefix_iterator(prefix)
    }
    pub fn save(&self, key: &str, config: &Value) -> Result<(), Error> {
        let raw = serde_json::to_vec(config)?;
        self.db.put(key, raw);
        self.db.flush().map_err(|e| e.into())
    }
}
