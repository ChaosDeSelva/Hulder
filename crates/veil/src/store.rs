//! Normal Hulder startup uses an in-memory session store. File-backed stores below are
//! retained for compatibility tests; normal startup does not read legacy veil.json files.
//!
//! The passkey store: `veil.json` in the config directory.
//!
//! Neither `argon2` nor `scrypt` is a workspace dependency, so the key derivation is
//! PBKDF2-HMAC-SHA256 with 600 000 rounds and a 16-byte salt from the OS random source. The
//! scheme name and round count are written into the file so a later move to Argon2id can
//! recognise the old format. The passkey itself is never written anywhere.
//!
//! The failure counter, the lockout deadline and the locked flag live in the same file, so a
//! restart neither resets the lockout nor lifts the veil, and `verify` refuses to compare at all
//! while the lockout is in force.
//!
//! The file is replaced atomically (written to a sibling temp file, synced, renamed over) so a
//! crash mid-write leaves the old record rather than half of a new one. It is opened without
//! following a symlink and read up to a small cap. A file that is present but cannot be read
//! or parsed is reported as [`StoreState::Damaged`], which counts as locked with a passkey
//! present: whoever can repair or delete the file is the owner, and nothing else lifts the veil.

use anyhow::{Context as _, Result, anyhow, bail};
use parking_lot::Mutex;
use rand::TryRngCore as _;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::Zeroizing;

const KDF_NAME: &str = "pbkdf2-hmac-sha256";
const ITERATIONS: u32 = 600_000;
const SALT_LEN: usize = 16;
const FAILURES_BEFORE_LOCKOUT: u32 = 3;
const FIRST_LOCKOUT: Duration = Duration::from_secs(10);
const MAX_LOCKOUT: Duration = Duration::from_secs(300);
/// A record is a few hundred bytes; anything past this is not a record.
const MAX_FILE_LEN: u64 = 64 * 1024;

/// Shown wherever a damaged `veil.json` stops an unlock.
pub const DAMAGED_MESSAGE: &str = "veil.json is damaged; restore it or delete it to reset";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasskeyRecord {
    version: u32,
    kdf: String,
    iterations: u32,
    salt: String,
    hash: String,
    #[serde(default)]
    failed_attempts: u32,
    /// Unix seconds before which `verify` refuses to compare; `0` when not locked out.
    #[serde(default)]
    locked_until: u64,
    /// Whether the veil was up when the app last ran, so a relaunch comes back veiled.
    #[serde(default)]
    locked: bool,
}

/// The result of one unlock attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    Verified,
    /// The passkey was wrong. `wait` is the lockout now in force, zero below three failures.
    Rejected {
        wait: Duration,
    },
    /// The passkey was not compared because a lockout is still in force.
    LockedOut {
        remaining: Duration,
    },
}

/// What `veil.json` said the last time it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreState {
    /// No file: no passkey has been set and there is nothing to lock.
    Missing,
    /// A readable record.
    Present {
        locked: bool,
        failed_attempts: u32,
        /// Unix seconds before which attempts are refused; `0` when not locked out.
        locked_until: u64,
    },
    /// The file exists but could not be read or parsed. Fails closed: locked, passkey present.
    Damaged,
}

impl StoreState {
    pub fn has_passkey(&self) -> bool {
        !matches!(self, StoreState::Missing)
    }

    pub fn is_locked(&self) -> bool {
        match self {
            StoreState::Missing => false,
            StoreState::Present { locked, .. } => *locked,
            StoreState::Damaged => true,
        }
    }

    pub fn is_damaged(&self) -> bool {
        matches!(self, StoreState::Damaged)
    }

    pub fn failed_attempts(&self) -> u32 {
        match self {
            StoreState::Present {
                failed_attempts, ..
            } => *failed_attempts,
            StoreState::Missing | StoreState::Damaged => 0,
        }
    }

    /// The lockout still in force now, if any.
    pub fn lockout_remaining(&self) -> Option<Duration> {
        self.lockout_remaining_at(unix_now())
    }

    fn lockout_remaining_at(&self, now: u64) -> Option<Duration> {
        match self {
            StoreState::Present { locked_until, .. } => remaining_lockout(*locked_until, now),
            StoreState::Missing | StoreState::Damaged => None,
        }
    }
}

/// How long attempts are refused after `failed_attempts` consecutive failures: nothing below
/// three, then 10 s doubling to a 5 minute ceiling.
pub fn lockout_duration(failed_attempts: u32) -> Duration {
    if failed_attempts < FAILURES_BEFORE_LOCKOUT {
        return Duration::ZERO;
    }
    let doublings = (failed_attempts - FAILURES_BEFORE_LOCKOUT).min(16);
    (FIRST_LOCKOUT * 2u32.pow(doublings)).min(MAX_LOCKOUT)
}

pub struct VeilStore {
    path: PathBuf,
    iterations: u32,
    /// Serialises read-modify-write cycles between the overlay and the passkey modal.
    file_lock: Mutex<()>,
    session_record: Option<Mutex<Option<PasskeyRecord>>>,
}

impl VeilStore {
    pub fn new(path: PathBuf) -> Self {
        Self::with_iterations(path, ITERATIONS)
    }

    /// A store with a cheaper key derivation, for tests that unlock many times.
    pub fn with_iterations(path: PathBuf, iterations: u32) -> Self {
        Self {
            path,
            iterations: iterations.max(1),
            file_lock: Mutex::new(()),
            session_record: None,
        }
    }

    /// A temporary veil: restarting the app discards the passkey and lock.
    pub fn session() -> Self {
        let mut store = Self::with_iterations(PathBuf::new(), ITERATIONS);
        store.session_record = Some(Mutex::new(None));
        store
    }

    pub fn is_session(&self) -> bool {
        self.session_record.is_some()
    }

    pub fn default_path() -> PathBuf {
        paths::config_dir().join("veil.json")
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the file once. Callers off the main thread should cache the result rather than
    /// call the convenience accessors below repeatedly.
    pub fn state(&self) -> StoreState {
        match self.read_record() {
            Ok(None) => StoreState::Missing,
            Ok(Some(record)) => StoreState::Present {
                locked: record.locked,
                failed_attempts: record.failed_attempts,
                locked_until: record.locked_until,
            },
            Err(error) => {
                log::error!("{DAMAGED_MESSAGE}: {error:#}");
                StoreState::Damaged
            }
        }
    }

    pub fn has_passkey(&self) -> bool {
        self.state().has_passkey()
    }

    pub fn failed_attempts(&self) -> u32 {
        self.state().failed_attempts()
    }

    /// The lockout still in force now, if any.
    pub fn lockout_remaining(&self) -> Option<Duration> {
        self.state().lockout_remaining()
    }

    pub fn set_passkey(&self, passkey: &str) -> Result<()> {
        if passkey.is_empty() {
            bail!("the passkey must be at least one character");
        }
        let _guard = self.file_lock.lock();
        let mut salt = [0u8; SALT_LEN];
        rand::rngs::OsRng
            .try_fill_bytes(&mut salt)
            .context("reading random salt")?;
        let hash = derive_hash(&salt, passkey.as_bytes(), self.iterations);
        let record = PasskeyRecord {
            version: 2,
            kdf: KDF_NAME.to_string(),
            iterations: self.iterations,
            salt: hex::encode(salt),
            hash: hex::encode(*hash),
            failed_attempts: 0,
            locked_until: 0,
            locked: false,
        };
        self.write_record(&record)
    }

    /// Whether the veil was up when the record was last written. A damaged file counts as
    /// locked.
    pub fn is_locked(&self) -> bool {
        self.state().is_locked()
    }

    pub fn set_locked(&self, locked: bool) -> Result<()> {
        let _guard = self.file_lock.lock();
        let Some(mut record) = self.read_record()? else {
            bail!("no passkey has been set");
        };
        if !locked {
            if let Some(session) = &self.session_record {
                *session.lock() = None;
                return Ok(());
            }
        }
        record.locked = locked;
        self.write_record(&record)
    }

    pub fn verify(&self, passkey: &str) -> Result<Verification> {
        self.verify_at(passkey, unix_now())
    }

    fn verify_at(&self, passkey: &str, now: u64) -> Result<Verification> {
        let _guard = self.file_lock.lock();
        let Some(mut record) = self.read_record()? else {
            bail!("no passkey has been set");
        };
        if let Some(remaining) = remaining_lockout(record.locked_until, now) {
            return Ok(Verification::LockedOut { remaining });
        }
        if record.kdf != KDF_NAME {
            bail!("unknown passkey scheme {:?}", record.kdf);
        }
        let salt = hex::decode(&record.salt).context("decoding salt")?;
        let expected = hex::decode(&record.hash).context("decoding hash")?;
        let actual = derive_hash(&salt, passkey.as_bytes(), record.iterations);
        if constant_time_eq(actual.as_slice(), &expected) {
            record.failed_attempts = 0;
            record.locked_until = 0;
            self.write_record(&record)?;
            return Ok(Verification::Verified);
        }
        record.failed_attempts = record.failed_attempts.saturating_add(1);
        let wait = lockout_duration(record.failed_attempts);
        record.locked_until = if wait.is_zero() {
            0
        } else {
            now.saturating_add(wait.as_secs())
        };
        self.write_record(&record)?;
        Ok(Verification::Rejected { wait })
    }

    /// `Ok(None)` only when there is no file at all; every other failure is an error, so a
    /// damaged file can never be mistaken for "no passkey".
    fn read_record(&self) -> Result<Option<PasskeyRecord>> {
        if let Some(session) = &self.session_record {
            return Ok(session.lock().clone());
        }
        let mut file = match fs::open_regular_file_nofollow(&self.path) {
            Ok(file) => file,
            Err(error) => {
                let not_found = error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
                if not_found {
                    return Ok(None);
                }
                return Err(error.context("opening veil.json"));
            }
        };
        let mut text = String::new();
        std::io::Read::by_ref(&mut file)
            .take(MAX_FILE_LEN + 1)
            .read_to_string(&mut text)
            .context("reading veil.json")?;
        if text.len() as u64 > MAX_FILE_LEN {
            return Err(anyhow!("veil.json is larger than {MAX_FILE_LEN} bytes"));
        }
        let record = serde_json::from_str(&text).context("parsing veil.json")?;
        Ok(Some(record))
    }

    /// Writes the record to a private temp file beside `veil.json`, syncs it, and renames it
    /// into place, so the file on disk is always either the old record or the whole new one.
    fn write_record(&self, record: &PasskeyRecord) -> Result<()> {
        if let Some(session) = &self.session_record {
            *session.lock() = Some(record.clone());
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("veil.json has no parent directory"))?;
        std::fs::create_dir_all(parent).context("creating config directory")?;
        let text = serde_json::to_string_pretty(record)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".veil-")
            .suffix(".json.tmp")
            .tempfile_in(parent)
            .context("creating a temp file beside veil.json")?;
        #[cfg(unix)]
        {
            // `tempfile` already creates with 0o600, but say so rather than rely on it.
            use std::os::unix::fs::PermissionsExt as _;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .context("setting veil.json permissions")?;
        }
        temp.write_all(text.as_bytes())
            .context("writing veil.json")?;
        temp.as_file().sync_all().context("syncing veil.json")?;
        temp.persist(&self.path)
            .map_err(|error| error.error)
            .context("replacing veil.json")?;
        Ok(())
    }
}

fn remaining_lockout(locked_until: u64, now: u64) -> Option<Duration> {
    (locked_until > now).then(|| Duration::from_secs(locked_until - now))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn derive_hash(salt: &[u8], passkey: &[u8], iterations: u32) -> Zeroizing<[u8; 32]> {
    let mut hash = Zeroizing::new([0u8; 32]);
    pbkdf2::pbkdf2_hmac::<Sha256>(passkey, salt, iterations.max(1), hash.as_mut_slice());
    hash
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left_byte, right_byte) in left.iter().zip(right) {
        difference |= left_byte ^ right_byte;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ITERATIONS: u32 = 16;

    fn temp_store() -> (tempfile::TempDir, VeilStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = VeilStore::with_iterations(
            dir.path().join("nested").join("veil.json"),
            TEST_ITERATIONS,
        );
        (dir, store)
    }

    #[test]
    fn pbkdf2_matches_known_vectors() {
        assert_eq!(
            hex::encode(*derive_hash(b"salt", b"password", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex::encode(*derive_hash(b"salt", b"password", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn round_trip() {
        let (_dir, store) = temp_store();
        assert!(!store.has_passkey());
        assert!(store.verify("hunter2").is_err());

        store.set_passkey("hunter2").expect("set");
        assert!(store.has_passkey());
        assert_eq!(
            store.verify("hunter2").expect("verify"),
            Verification::Verified
        );
        assert_eq!(
            store.verify("hunter3").expect("verify"),
            Verification::Rejected {
                wait: Duration::ZERO
            }
        );
        assert_eq!(store.failed_attempts(), 1);
        assert_eq!(
            store.verify("hunter2").expect("verify"),
            Verification::Verified
        );
        assert_eq!(store.failed_attempts(), 0);
    }

    #[test]
    fn locked_flag_survives_a_restart() {
        let (_dir, store) = temp_store();
        assert!(store.set_locked(true).is_err());
        store.set_passkey("hunter2").expect("set");
        assert!(!store.is_locked());
        store.set_locked(true).expect("lock");
        let reopened = VeilStore::with_iterations(store.path().to_path_buf(), TEST_ITERATIONS);
        assert!(reopened.is_locked());
        reopened.set_locked(false).expect("unlock");
        assert!(!store.is_locked());
    }

    #[test]
    fn empty_passkey_is_rejected() {
        let (_dir, store) = temp_store();
        assert!(store.set_passkey("").is_err());
        assert!(!store.has_passkey());
    }

    #[test]
    fn lockout_grows_from_ten_seconds_to_five_minutes() {
        assert_eq!(lockout_duration(0), Duration::ZERO);
        assert_eq!(lockout_duration(2), Duration::ZERO);
        assert_eq!(lockout_duration(3), Duration::from_secs(10));
        assert_eq!(lockout_duration(4), Duration::from_secs(20));
        assert_eq!(lockout_duration(5), Duration::from_secs(40));
        assert_eq!(lockout_duration(8), Duration::from_secs(300));
        assert_eq!(lockout_duration(60), Duration::from_secs(300));
    }

    #[test]
    fn lockout_is_enforced_before_comparing_and_survives_a_restart() {
        let (dir, store) = temp_store();
        store.set_passkey("hunter2").expect("set");
        let now = 1_000_000;
        for _ in 0..2 {
            assert_eq!(
                store.verify_at("nope", now).expect("verify"),
                Verification::Rejected {
                    wait: Duration::ZERO
                }
            );
        }
        assert_eq!(
            store.verify_at("nope", now).expect("verify"),
            Verification::Rejected {
                wait: Duration::from_secs(10)
            }
        );

        // Even the right passkey is refused without comparison while locked out.
        let reopened = VeilStore::with_iterations(store.path().to_path_buf(), TEST_ITERATIONS);
        assert_eq!(
            reopened.verify_at("hunter2", now + 4).expect("verify"),
            Verification::LockedOut {
                remaining: Duration::from_secs(6)
            }
        );
        assert_eq!(reopened.failed_attempts(), 3);

        // The next failure after the lockout doubles it.
        assert_eq!(
            reopened.verify_at("nope", now + 10).expect("verify"),
            Verification::Rejected {
                wait: Duration::from_secs(20)
            }
        );
        assert_eq!(
            reopened.verify_at("hunter2", now + 29).expect("verify"),
            Verification::LockedOut {
                remaining: Duration::from_secs(1)
            }
        );
        assert_eq!(
            reopened.verify_at("hunter2", now + 30).expect("verify"),
            Verification::Verified
        );
        assert_eq!(reopened.failed_attempts(), 0);
        assert!(reopened.lockout_remaining().is_none());
        drop(dir);
    }

    #[test]
    fn file_never_contains_the_passkey_and_salts_differ() {
        let (_dir, store) = temp_store();
        store.set_passkey("correct horse").expect("set");
        let first = std::fs::read_to_string(store.path()).expect("read");
        assert!(!first.contains("correct horse"));
        assert!(first.contains(KDF_NAME));

        store.set_passkey("correct horse").expect("set again");
        let second = std::fs::read_to_string(store.path()).expect("read");
        assert_ne!(first, second);
        assert_eq!(
            store.verify("correct horse").expect("verify"),
            Verification::Verified
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private_even_when_it_already_existed() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().expect("parent")).expect("mkdir");
        std::fs::write(store.path(), "{}").expect("seed");
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644))
            .expect("chmod");

        store.set_passkey("hunter2").expect("set");
        let mode = std::fs::metadata(store.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn constant_time_eq_compares_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    fn seed_file(store: &VeilStore, contents: &[u8]) {
        std::fs::create_dir_all(store.path().parent().expect("parent")).expect("mkdir");
        std::fs::write(store.path(), contents).expect("seed");
    }

    #[test]
    fn a_damaged_file_counts_as_locked_with_a_passkey_present() {
        let (_dir, store) = temp_store();
        seed_file(&store, b"{\"version\": 2, \"kdf\": \"pbkdf2-hm");
        assert_eq!(store.state(), StoreState::Damaged);
        assert!(store.has_passkey());
        assert!(store.is_locked());
        assert!(store.lockout_remaining().is_none());
        let error = store
            .verify("hunter2")
            .expect_err("a damaged file must not verify");
        assert!(error.to_string().contains("parsing veil.json"), "{error:#}");
        assert!(store.set_locked(false).is_err());
    }

    #[test]
    fn an_oversized_file_is_damaged() {
        let (_dir, store) = temp_store();
        seed_file(&store, &vec![b' '; MAX_FILE_LEN as usize + 1]);
        assert_eq!(store.state(), StoreState::Damaged);
        assert!(store.is_locked());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_damaged() {
        let (dir, store) = temp_store();
        std::fs::create_dir_all(store.path().parent().expect("parent")).expect("mkdir");
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, "{}").expect("target");
        std::os::unix::fs::symlink(&target, store.path()).expect("symlink");
        assert_eq!(store.state(), StoreState::Damaged);
        assert!(store.is_locked());
    }

    #[test]
    fn a_missing_file_means_no_passkey_and_unlocked() {
        let (_dir, store) = temp_store();
        assert_eq!(store.state(), StoreState::Missing);
        assert!(!store.has_passkey());
        assert!(!store.is_locked());
    }

    #[test]
    fn writes_replace_the_file_whole_and_leave_no_temp_file() {
        let (_dir, store) = temp_store();
        store.set_passkey("hunter2").expect("set");
        store.set_locked(true).expect("lock");
        let parent = store.path().parent().expect("parent");
        let names: Vec<String> = std::fs::read_dir(parent)
            .expect("read dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec!["veil.json".to_string()],
            "temp files were left behind"
        );
        assert_eq!(
            store.state(),
            StoreState::Present {
                locked: true,
                failed_attempts: 0,
                locked_until: 0
            }
        );
    }

    #[test]
    fn state_reports_the_lockout() {
        let state = StoreState::Present {
            locked: true,
            failed_attempts: 3,
            locked_until: 1_000,
        };
        assert_eq!(
            state.lockout_remaining_at(994),
            Some(Duration::from_secs(6))
        );
        assert_eq!(state.lockout_remaining_at(1_000), None);
        assert_eq!(state.failed_attempts(), 3);
        assert!(state.is_locked());
        assert!(state.has_passkey());
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    #[test]
    fn each_lock_has_a_new_password_and_restart_is_recovery() {
        let mut store = VeilStore::session();
        store.iterations = 1;
        store.set_passkey("first").expect("set first");
        store.set_locked(true).expect("lock");
        assert!(matches!(
            store.verify("wrong").expect("verify"),
            Verification::Rejected { .. }
        ));
        assert_eq!(
            store.verify("first").expect("verify"),
            Verification::Verified
        );
        store.set_locked(false).expect("unlock");
        assert_eq!(store.state(), StoreState::Missing);
        store.set_passkey("second").expect("set second");
        assert!(matches!(
            store.verify("first").expect("verify old"),
            Verification::Rejected { .. }
        ));
        assert_eq!(
            store.verify("second").expect("verify new"),
            Verification::Verified
        );
        assert_eq!(VeilStore::session().state(), StoreState::Missing);
        assert!(store.path().as_os_str().is_empty());
    }
}
