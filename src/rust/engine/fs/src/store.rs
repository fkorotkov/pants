use bazel_protos;
use boxfuture::BoxFuture;
use protobuf::core::Message;
use std::path::Path;
use std::sync::Arc;

use hash::Fingerprint;
use pool::ResettablePool;

///
/// A Digest is a fingerprint, as well as the size in bytes of the plaintext for which that is the
/// fingerprint.
///
/// It is equivalent to a Bazel Remote Execution Digest, but without the overhead (and awkward API)
/// of needing to create an entire protobuf to pass around the two fields.
///
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Digest(pub Fingerprint, pub usize);

impl Into<bazel_protos::remote_execution::Digest> for Digest {
  fn into(self) -> bazel_protos::remote_execution::Digest {
    let mut digest = bazel_protos::remote_execution::Digest::new();
    digest.set_hash(self.0.to_hex());
    digest.set_size_bytes(self.1 as i64);
    digest
  }
}

///
/// A content-addressed store of file contents, and Directories.
///
/// Currently, Store only stores things locally on disk, but in the future it will gain the ability
/// to fetch files from remote content-addressable storage too.
///
#[derive(Clone)]
pub struct Store {
  local: local::ByteStore,
}

impl Store {
  pub fn new<P: AsRef<Path>>(path: P, pool: Arc<ResettablePool>) -> Result<Store, String> {
    Ok(Store { local: local::ByteStore::new(path, pool)? })
  }

  pub fn store_file_bytes(&self, bytes: Vec<u8>) -> BoxFuture<Digest, String> {
    self.local.store_file_bytes(bytes)
  }

  pub fn load_file_bytes_with<T: Send + 'static, F: Fn(&[u8]) -> T + Send + Sync + 'static>(
    &self,
    fingerprint: Fingerprint,
    f: F,
  ) -> BoxFuture<Option<T>, String> {
    self.local.load_file_bytes_with(
      fingerprint.clone(),
      Arc::new(f),
    )
  }

  pub fn record_directory(
    &self,
    directory: &bazel_protos::remote_execution::Directory,
  ) -> BoxFuture<Digest, String> {
    self.local.record_directory(directory)
  }

  pub fn load_directory(
    &self,
    fingerprint: Fingerprint,
  ) -> BoxFuture<Option<bazel_protos::remote_execution::Directory>, String> {
    self.local.load_directory_proto_bytes_with(
      fingerprint,
      Arc::new(|bytes: &[u8]| {
        let mut directory = bazel_protos::remote_execution::Directory::new();
        directory.merge_from_bytes(bytes).expect(
          "LMDB corruption: Directory bytes were not valid",
        );
        directory
      }),
    )
  }
}

///
/// ByteStore allows read and write access to byte-buffers of two types:
/// 1. File contents (arbitrary blobs with no particular assumed structure).
/// 2. Directory protos, serialized using the standard protobuf binary serialization.
///
pub trait ByteStore {
  fn store_file_bytes(&self, bytes: Vec<u8>) -> BoxFuture<Digest, String>;

  fn load_file_bytes_with<T: Send + 'static, F: Fn(&[u8]) -> T + Send + Sync + 'static>(
    &self,
    fingerprint: Fingerprint,
    f: Arc<F>,
  ) -> BoxFuture<Option<T>, String>;

  fn record_directory(
    &self,
    directory: &bazel_protos::remote_execution::Directory,
  ) -> BoxFuture<Digest, String>;

  ///
  /// Load a Directory proto's bytes. This function will only return canonical Directory protos.
  ///
  fn load_directory_proto_bytes_with<T: Send + 'static, F: Fn(&[u8]) -> T + Send + Sync + 'static>(
    &self,
    fingerprint: Fingerprint,
    f: Arc<F>,
  ) -> BoxFuture<Option<T>, String>;
}

mod local {
  use super::Digest;

  use bazel_protos;
  use boxfuture::{Boxable, BoxFuture};
  use digest::{Digest as DigestTrait, FixedOutput};
  use futures::{future, Future};
  use futures_cpupool::CpuFuture;
  use lmdb::{Database, DatabaseFlags, Environment, NO_OVERWRITE, Transaction};
  use lmdb::Error::{KeyExist, NotFound};
  use protobuf::core::Message;
  use sha2::Sha256;
  use std::error::Error;
  use std::path::Path;
  use std::sync::Arc;

  use hash::Fingerprint;
  use pool::ResettablePool;

  #[derive(Clone)]
  pub struct ByteStore {
    inner: Arc<InnerStore>,
  }

  struct InnerStore {
    env: Environment,
    pool: Arc<ResettablePool>,
    file_store: Database,
    // Store directories separately from files because:
    //  1. They may have different lifetimes.
    //  2. It's nice to know whether we should be able to parse something as a proto.
    directory_store: Database,
  }

  impl ByteStore {
    pub fn new<P: AsRef<Path>>(path: P, pool: Arc<ResettablePool>) -> Result<ByteStore, String> {
      // 2 DBs; one for file contents, one for directories.
      let env = Environment::new()
        .set_max_dbs(2)
        .set_map_size(16 * 1024 * 1024 * 1024)
        .open(path.as_ref())
        .map_err(|e| format!("Error making env: {}", e.description()))?;
      let file_database = env
        .create_db(Some("files"), DatabaseFlags::empty())
        .map_err(|e| {
          format!("Error creating/opening files database: {}", e.description())
        })?;
      let directory_database = env
        .create_db(Some("directories"), DatabaseFlags::empty())
        .map_err(|e| {
          format!(
            "Error creating/opening directories database: {}",
            e.description()
          )
        })?;
      Ok(ByteStore {
        inner: Arc::new(InnerStore {
          env: env,
          pool: pool,
          file_store: file_database,
          directory_store: directory_database,
        }),
      })
    }

    fn load_bytes_with<T: Send + 'static, F: Fn(&[u8]) -> T + Send + Sync + 'static>(
      &self,
      fingerprint: Fingerprint,
      db: Database,
      f: Arc<F>,
    ) -> CpuFuture<Option<T>, String> {
      let store = self.inner.clone();
      self.inner.pool.spawn_fn(move || {
        let ro_txn = store.env.begin_ro_txn().map_err(|err| {
          format!(
            "Failed to begin read transaction: {}",
            err.description().to_string()
          )
        });
        ro_txn.and_then(|txn| match txn.get(db, &fingerprint) {
          Ok(bytes) => Ok(Some(f(bytes))),
          Err(NotFound) => Ok(None),
          Err(err) => Err(format!(
            "Error loading fingerprint {}: {}",
            fingerprint,
            err.description().to_string()
          )),
        })
      })
    }

    fn store_bytes(&self, bytes: Vec<u8>, db: Database) -> CpuFuture<Fingerprint, String> {
      let store = self.clone();
      self.inner.pool.spawn_fn(move || {
        let fingerprint = {
          let mut hasher = Sha256::default();
          hasher.input(&bytes);
          Fingerprint::from_bytes_unsafe(hasher.fixed_result().as_slice())
        };

        let put_res = store.inner.env.begin_rw_txn().and_then(|mut txn| {
          txn.put(db, &fingerprint, &bytes, NO_OVERWRITE).and_then(
            |()| txn.commit(),
          )
        });

        match put_res {
          Ok(()) => Ok(fingerprint),
          Err(KeyExist) => Ok(fingerprint),
          Err(err) => Err(format!(
            "Error storing fingerprint {}: {}",
            fingerprint,
            err.description()
          )),
        }
      })
    }
  }

  impl super::ByteStore for ByteStore {
    fn store_file_bytes(&self, bytes: Vec<u8>) -> BoxFuture<Digest, String> {
      let len = bytes.len();
      self
        .store_bytes(bytes, self.inner.file_store.clone())
        .map(move |fingerprint| Digest(fingerprint, len))
        .to_boxed()
    }

    fn load_file_bytes_with<T: Send + 'static, F: Fn(&[u8]) -> T + Send + Sync + 'static>(
      &self,
      fingerprint: Fingerprint,
      f: Arc<F>,
    ) -> BoxFuture<Option<T>, String> {
      self
        .load_bytes_with(fingerprint, self.inner.file_store, f)
        .to_boxed()
    }

    ///
    /// Store the Directory proto. Does not do anything about the files or directories claimed to be
    /// contained therein.
    ///
    /// Assumes that the directory has been properly canonicalized.
    ///
    fn record_directory(
      &self,
      directory: &bazel_protos::remote_execution::Directory,
    ) -> BoxFuture<Digest, String> {
      let store = self.clone();
      future::result(directory.write_to_bytes().map_err(|e| {
        format!(
          "Error serializing directory proto {:?}: {}",
          directory,
          e.description()
        )
      })).and_then(move |bytes| {
        let len = bytes.len();

        store
          .store_bytes(bytes, store.inner.directory_store.clone())
          .map(move |fingerprint| Digest(fingerprint, len))
      })
        .to_boxed()
    }

    fn load_directory_proto_bytes_with<
      T: Send + 'static,
      F: Fn(&[u8]) -> T + Send + Sync + 'static,
    >(
      &self,
      fingerprint: Fingerprint,
      f: Arc<F>,
    ) -> BoxFuture<Option<T>, String> {
      self
        .load_bytes_with(fingerprint.clone(), self.inner.directory_store, f)
        .to_boxed()
    }
  }

  #[cfg(test)]
  mod tests {
    extern crate tempdir;

    use futures::Future;
    use super::{ByteStore, Fingerprint, ResettablePool};
    use super::super::ByteStore as _ByteStore;
    use lmdb::{DatabaseFlags, Environment, Transaction, WriteFlags};
    use protobuf::Message;
    use std::path::Path;
    use std::sync::Arc;
    use tempdir::TempDir;

    use super::super::tests::{DIRECTORY_HASH, HASH, digest, directory, directory_fingerprint,
                              fingerprint, str_bytes};

    #[test]
    fn save_file() {
      let dir = TempDir::new("store").unwrap();

      assert_eq!(
        new_store(dir.path()).store_file_bytes(str_bytes()).wait(),
        Ok(digest())
      );
    }

    #[test]
    fn save_file_is_idempotent() {
      let dir = TempDir::new("store").unwrap();

      new_store(dir.path())
        .store_file_bytes(str_bytes())
        .wait()
        .unwrap();
      assert_eq!(
        new_store(dir.path()).store_file_bytes(str_bytes()).wait(),
        Ok(digest())
      );
    }

    #[test]
    fn save_file_collision_preserves_first() {
      let dir = TempDir::new("store").unwrap();

      let fingerprint = Fingerprint::from_hex_string(HASH).unwrap();
      let bogus_value: Vec<u8> = vec![];

      let env = Environment::new().set_max_dbs(1).open(dir.path()).unwrap();
      let database = env.create_db(Some("files"), DatabaseFlags::empty());
      env
        .begin_rw_txn()
        .and_then(|mut txn| {
          txn.put(database.unwrap(), &fingerprint, &bogus_value, WriteFlags::empty())
                .and_then(|()| txn.commit())
        })
        .unwrap();

      assert_eq!(
        load_file_bytes(&new_store(dir.path()), fingerprint),
        Ok(Some(bogus_value.clone()))
      );

      assert_eq!(
        new_store(dir.path()).store_file_bytes(str_bytes()).wait(),
        Ok(digest())
      );

      assert_eq!(
        load_file_bytes(&new_store(dir.path()), fingerprint),
        Ok(Some(bogus_value.clone()))
      );
    }

    #[test]
    fn roundtrip_file() {
      let data = str_bytes();
      let dir = TempDir::new("store").unwrap();

      let store = new_store(dir.path());
      let hash = store.store_file_bytes(data.clone()).wait().unwrap();
      assert_eq!(load_file_bytes(&store, hash.0), Ok(Some(data)));
    }

    #[test]
    fn missing_file() {
      let dir = TempDir::new("store").unwrap();
      assert_eq!(
        load_file_bytes(&new_store(dir.path()), fingerprint()),
        Ok(None)
      );
    }

    #[test]
    fn record_and_load_directory_proto() {
      let dir = TempDir::new("store").unwrap();

      assert_eq!(
        &new_store(dir.path())
          .record_directory(&directory())
          .wait()
          .unwrap()
          .0
          .to_hex(),
        DIRECTORY_HASH
      );

      assert_eq!(
        load_directory_proto_bytes(&new_store(dir.path()), directory_fingerprint()),
        Ok(Some(directory().write_to_bytes().unwrap()))
      );
    }

    #[test]
    fn missing_directory() {
      let dir = TempDir::new("store").unwrap();

      assert_eq!(
        load_directory_proto_bytes(&new_store(dir.path()), directory_fingerprint()),
        Ok(None)
      );
    }

    #[test]
    fn file_is_not_directory_proto() {
      let dir = TempDir::new("store").unwrap();

      new_store(dir.path())
        .store_file_bytes(str_bytes())
        .wait()
        .unwrap();

      assert_eq!(
        load_directory_proto_bytes(&new_store(dir.path()), fingerprint()),
        Ok(None)
      );
    }

    fn new_store<P: AsRef<Path>>(dir: P) -> ByteStore {
      ByteStore::new(dir, Arc::new(ResettablePool::new("test-pool-".to_string()))).unwrap()
    }

    fn load_file_bytes(
      store: &ByteStore,
      fingerprint: Fingerprint,
    ) -> Result<Option<Vec<u8>>, String> {
      store
        .load_file_bytes_with(fingerprint, Arc::new(|bytes: &[u8]| bytes.to_vec()))
        .wait()
    }

    fn load_directory_proto_bytes(
      store: &ByteStore,
      fingerprint: Fingerprint,
    ) -> Result<Option<Vec<u8>>, String> {
      store
        .load_directory_proto_bytes_with(fingerprint, Arc::new(|bytes: &[u8]| bytes.to_vec()))
        .wait()
    }
  }
}

#[cfg(test)]
mod tests {
  use bazel_protos;
  use super::{Digest, Fingerprint};

  pub const STR: &str = "European Burmese";
  pub const HASH: &str = "693d8db7b05e99c6b7a7c0616456039d89c555029026936248085193559a0b5d";
  pub const DIRECTORY_HASH: &str = "63949aa823baf765eff07b946050d76e\
c0033144c785a94d3ebd82baa931cd16";

  pub fn fingerprint() -> Fingerprint {
    Fingerprint::from_hex_string(HASH).unwrap()
  }

  pub fn digest() -> Digest {
    Digest(fingerprint(), STR.len())
  }

  pub fn str_bytes() -> Vec<u8> {
    STR.as_bytes().to_owned()
  }

  pub fn directory() -> bazel_protos::remote_execution::Directory {
    let mut directory = bazel_protos::remote_execution::Directory::new();
    directory.mut_files().push({
      let mut file = bazel_protos::remote_execution::FileNode::new();
      file.set_name("roland".to_string());
      file.set_digest({
        let mut digest = bazel_protos::remote_execution::Digest::new();
        digest.set_hash(HASH.to_string());
        digest.set_size_bytes(STR.len() as i64);
        digest
      });
      file.set_is_executable(false);
      file
    });
    directory
  }

  pub fn directory_fingerprint() -> Fingerprint {
    Fingerprint::from_hex_string(DIRECTORY_HASH).unwrap()
  }

  #[test]
  fn digest_to_bazel_digest() {
    let digest = Digest(Fingerprint::from_hex_string(HASH).unwrap(), 16);
    let mut bazel_digest = bazel_protos::remote_execution::Digest::new();
    bazel_digest.set_hash(HASH.to_string());
    bazel_digest.set_size_bytes(16);
    assert_eq!(bazel_digest, digest.into());
  }
}
