/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::upgrades::UpgradeKind;
use super::{bundle::ToLocalReason, meta, LocalRecord, NativeRecord, SchemaBundle, SyncStatus};
use crate::error::*;
use crate::ms_time::MsTime;
use crate::sync::schema_action::UpgradeRemote;
use crate::vclock::{Counter, VClock};
use crate::Guid;
use crate::RecordSchema;
use rusqlite::{named_params, Connection};
use sql_support::{ConnExt, SqlInterruptHandle, SqlInterruptScope};
use std::convert::TryFrom;
use std::sync::{atomic::AtomicUsize, Arc, Mutex};

pub struct RemergeDb {
    db: Connection,
    info: SchemaBundle,
    client_id: sync_guid::Guid,
    interrupt_counter: Arc<AtomicUsize>,
}

lazy_static::lazy_static! {
    // XXX: We should replace this with something like the PlacesApi path-based
    // hashmap, but for now this is better than nothing.
    static ref DB_INIT_MUTEX: Mutex<()> = Mutex::new(());
}

impl RemergeDb {
    pub(crate) fn with_connection(mut db: Connection, native: Arc<RecordSchema>) -> Result<Self> {
        let _g = DB_INIT_MUTEX.lock().unwrap();
        let pragmas = "
            -- The value we use was taken from Desktop Firefox, and seems necessary to
            -- help ensure good performance. The default value is 1024, which the SQLite
            -- docs themselves say is too small and should be changed.
            PRAGMA page_size = 32768;

            -- Disable calling mlock/munlock for every malloc/free.
            -- In practice this results in a massive speedup, especially
            -- for insert-heavy workloads.
            PRAGMA cipher_memory_security = false;

            -- `temp_store = 2` is required on Android to force the DB to keep temp
            -- files in memory, since on Android there's no tmp partition. See
            -- https://github.com/mozilla/mentat/issues/505. Ideally we'd only
            -- do this on Android, and/or allow caller to configure it.
            -- (although see also bug 1313021, where Firefox enabled it for both
            -- Android and 64bit desktop builds)
            PRAGMA temp_store = 2;

            -- We want foreign-key support.
            PRAGMA foreign_keys = ON;

            -- we unconditionally want write-ahead-logging mode
            PRAGMA journal_mode=WAL;

            -- How often to autocheckpoint (in units of pages).
            -- 2048000 (our max desired WAL size) / 32768 (page size).
            PRAGMA wal_autocheckpoint=62
        ";
        db.execute_batch(pragmas)?;
        let tx = db.transaction()?;
        super::schema::init(&tx)?;
        let (info, client_id) = super::bootstrap::load_or_bootstrap(&tx, native)?;
        tx.commit()?;
        Ok(RemergeDb {
            db,
            info,
            client_id,
            interrupt_counter: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn conn(&self) -> &rusqlite::Connection {
        &self.db
    }
    pub fn collection(&self) -> &str {
        &self.info.collection_name
    }
    pub fn info(&self) -> &SchemaBundle {
        &self.info
    }

    pub fn exists(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row_named(
            "SELECT EXISTS(
                 SELECT 1 FROM rec_local
                 WHERE guid = :guid AND is_deleted = 0
                 UNION ALL
                 SELECT 1 FROM rec_mirror
                 WHERE guid = :guid AND is_overridden IS NOT 1
             )",
            named_params! { ":guid": id },
            |row| row.get(0),
        )?)
    }

    pub fn create(&self, native: &NativeRecord) -> Result<Guid> {
        let (id, record) = self
            .info
            .native_to_local(&native, ToLocalReason::Creation)?;
        let tx = self.db.unchecked_transaction()?;
        // TODO: Search DB for dupes based on the value of the fields listed in dedupe_on.
        let id_exists = self.exists(id.as_ref())?;
        if id_exists {
            throw!(InvalidRecord::IdNotUnique);
        }
        if self.dupe_exists(&record)? {
            throw!(InvalidRecord::Duplicate);
        }
        let ctr = self.counter_bump()?;
        let vclock = VClock::new(self.client_id(), ctr);

        let now = MsTime::now();
        self.db.execute_named(
            "INSERT INTO rec_local (
                guid,
                remerge_schema_version,
                record_data,
                local_modified_ms,
                is_deleted,
                sync_status,
                vector_clock,
                last_writer_id
            ) VALUES (
                :guid,
                :schema_ver,
                :record,
                :now,
                0,
                :status,
                :vclock,
                :client_id
            )",
            named_params! {
                ":guid": id,
                ":schema_ver": self.info.local.version.to_string(),
                ":record": record,
                ":now": now,
                ":status": SyncStatus::New as u8,
                ":vclock": vclock,
                ":client_id": self.client_id,
            },
        )?;
        tx.commit()?;
        Ok(id)
    }

    fn counter_bump(&self) -> Result<Counter> {
        let mut ctr = meta::get::<i64>(&self.db, meta::CHANGE_COUNTER)?;
        assert!(
            ctr >= 0,
            "Corrupt db? negative global change counter: {:?}",
            ctr
        );
        ctr += 1;
        meta::put(&self.db, meta::CHANGE_COUNTER, &ctr)?;
        // Overflowing i64 takes around 9 quintillion (!!) writes, so the only
        // way it can realistically happen is on db corruption.
        //
        // FIXME: We should be returning a specific error for DB corruption
        // instead of panicing, and have a maintenance routine (a la places).
        Ok(Counter::try_from(ctr).expect("Corrupt db? i64 overflow"))
    }

    fn get_vclock(&self, id: &str) -> Result<VClock> {
        Ok(self.db.query_row_named(
            "SELECT vector_clock FROM rec_local
             WHERE guid = :guid AND is_deleted = 0
             UNION ALL
             SELECT vector_clock FROM rec_mirror
             WHERE guid = :guid AND is_overridden IS NOT 1",
            named_params! { ":guid": id },
            |row| row.get(0),
        )?)
    }

    pub fn delete_by_id(&self, id: &str) -> Result<bool> {
        let tx = self.db.unchecked_transaction()?;
        let exists = self.exists(id)?;
        if !exists {
            // Hrm, is there anything else we should do here? Logins goes
            // through the whole process (which is tricker for us...)
            return Ok(false);
        }
        let now_ms = MsTime::now();
        let vclock = self.get_bumped_vclock(id)?;

        // Locally, mark is_deleted and clear sensitive fields
        self.db.execute_named(
            "UPDATE rec_local
             SET local_modified_ms = :now_ms,
                 sync_status = :changed,
                 is_deleted = 1,
                 record_data = '{}',
                 vector_clock = :vclock,
                 last_writer_id = :own_id
             WHERE guid = :guid",
            named_params! {
                ":now_ms": now_ms,
                ":changed": SyncStatus::Changed as u8,
                ":guid": id,
                ":vclock": vclock,
                ":own_id": self.client_id,
            },
        )?;

        // Mark the mirror as overridden. XXX should we clear `record_data` here too?
        self.db.execute_named(
            "UPDATE rec_mirror SET is_overridden = 1 WHERE guid = :guid",
            named_params! { ":guid": id },
        )?;

        // If we don't have a local record for this ID, but do have it in the
        // mirror, insert tombstone.
        self.db.execute_named(
            "INSERT OR IGNORE INTO rec_local
                    (guid, local_modified_ms, is_deleted, sync_status, record_data, vector_clock, last_writer_id, remerge_schema_version)
             SELECT guid, :now_ms,           1,          :changed,    '{}',        :vclock,      :own_id,        :schema_ver
             FROM rec_mirror
             WHERE guid = :guid",
            named_params! {
                ":now_ms": now_ms,
                ":guid": id,
                ":schema_ver": self.info.local.version.to_string(),
                ":vclock": vclock,
                ":changed": SyncStatus::Changed as u8,
            })?;
        tx.commit()?;
        Ok(exists)
    }

    fn get_local_by_id(&self, id: &str) -> Result<Option<LocalRecord>> {
        Ok(self.db.try_query_row(
            "SELECT record_data FROM rec_local WHERE guid = :guid AND is_deleted = 0
             UNION ALL
             SELECT record_data FROM rec_mirror WHERE guid = :guid AND is_overridden = 0
             LIMIT 1",
            named_params! { ":guid": id },
            |r| r.get(0),
            true, // cache
        )?)
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<NativeRecord>> {
        self.get_local_by_id(id)?
            .map(|v| self.info.local_to_native(&v))
            .transpose()
    }

    pub fn get_all(&self) -> Result<Vec<NativeRecord>> {
        let mut stmt = self.db.prepare_cached(
            "SELECT record_data FROM rec_local WHERE is_deleted = 0
             UNION ALL
             SELECT record_data FROM rec_mirror WHERE is_overridden = 0",
        )?;
        let rows = stmt.query_and_then(rusqlite::NO_PARAMS, |row| -> Result<NativeRecord> {
            let r: LocalRecord = row.get("record_data")?;
            self.info.local_to_native(&r)
        })?;
        rows.collect::<Result<_>>()
    }

    fn ensure_local_overlay_exists(&self, guid: &str) -> Result<()> {
        let already_have_local: bool = self.db.query_row_named(
            "SELECT EXISTS(SELECT 1 FROM rec_local WHERE guid = :guid)",
            named_params! { ":guid": guid },
            |row| row.get(0),
        )?;

        if already_have_local {
            return Ok(());
        }

        log::debug!("No overlay; cloning one for {:?}.", guid);
        self.clone_mirror_to_overlay(guid)
    }

    // Note: unlike the version of this function in `logins`, we return Err if
    // `guid` is invalid instead of expecting the caller to check
    fn clone_mirror_to_overlay(&self, guid: &str) -> Result<()> {
        let sql = "
            INSERT OR IGNORE INTO rec_local
                (guid, record_data, vector_clock, last_writer_id, local_modified_ms, is_deleted, sync_status)
            SELECT
                 guid, record_data, vector_clock, last_writer_id, 0 as local_modified_ms, 0 AS is_deleted, 0 AS sync_status
            FROM rec_mirror
            WHERE guid = :guid
        ";
        let changed = self
            .db
            .execute_named_cached(sql, named_params! { ":guid": guid })?;

        if changed == 0 {
            log::error!("Failed to create local overlay for GUID {:?}.", guid);
            throw!(ErrorKind::NoSuchRecord(guid.to_owned()));
        }
        Ok(())
    }

    fn mark_mirror_overridden(&self, guid: &str) -> Result<()> {
        self.db.execute_named_cached(
            "UPDATE rec_mirror SET is_overridden = 1 WHERE guid = :guid",
            named_params! { ":guid": guid },
        )?;
        Ok(())
    }

    /// Combines get_vclock with counter_bump, and produces a new VClock with the bumped counter.
    fn get_bumped_vclock(&self, id: &str) -> Result<VClock> {
        let vc = self.get_vclock(id)?;
        let counter = self.counter_bump()?;
        Ok(vc.apply(self.client_id.clone(), counter))
    }

    /// Returns NoSuchRecord if, well, there's no such record.
    fn get_existing_record(&self, rec: &NativeRecord) -> Result<LocalRecord> {
        use crate::{
            schema::desc::{Field, FieldType},
            JsonValue,
        };
        let native = self.info.native_schema();
        let field = native.own_guid();
        assert!(
            matches::matches!(field.ty, FieldType::OwnGuid { .. }),
            "Validation/parsing bug -- field_own_guid must point to an own_guid"
        );
        // Just treat missing and null the same.
        let val = rec.get(&field.local_name).unwrap_or(&JsonValue::Null);
        let guid = Field::validate_guid(&field.local_name, val)?;

        self.get_local_by_id(guid.as_str())?
            .ok_or_else(|| ErrorKind::NoSuchRecord(guid.into()).into())
    }

    pub fn update_record(&self, record: &NativeRecord) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;

        // fails with NoSuchRecord if the record doesn't exist.

        // Potential optimization: we could skip this for schemas that don't use
        // types which need `prev` (untyped_map, record_set, ...)
        let prev = self.get_existing_record(&record)?;

        let (guid, record) = self
            .info
            .native_to_local(record, ToLocalReason::Update { prev })?;

        if self.dupe_exists(&record)? {
            throw!(InvalidRecord::Duplicate);
        }

        // Note: These fail with NoSuchRecord if the record doesn't exist.
        self.ensure_local_overlay_exists(guid.as_str())?;
        self.mark_mirror_overridden(guid.as_str())?;

        let now_ms = MsTime::now();

        let vclock = self.get_bumped_vclock(&guid)?;

        let sql = "
            UPDATE rec_local
            SET local_modified_ms      = :now_millis,
                record_data            = :record,
                vector_clock           = :vclock,
                last_writer_id         = :own_id,
                remerge_schema_version = :schema_ver,
                sync_status            = max(sync_status, :changed)
            WHERE guid = :guid
        ";

        let ct = self.db.execute_named(
            &sql,
            named_params! {
                ":guid": guid,
                ":changed": SyncStatus::Changed as u8,
                ":record": record,
                ":schema_ver": self.info.local.version.to_string(),
                ":now_millis": now_ms,
                ":own_id": self.client_id,
                ":vclock": vclock,
            },
        )?;
        debug_assert_eq!(ct, 1);
        tx.commit()?;
        Ok(())
    }

    pub fn client_id(&self) -> Guid {
        // Guid are essentially free unless the Guid ends up in the "large guid"
        // path, which should never happen for remerge client ids, so it should
        // be fine to always clone this.
        self.client_id.clone()
    }

    pub fn bundle(&self) -> &SchemaBundle {
        &self.info
    }

    fn dupe_exists(&self, _record: &LocalRecord) -> Result<bool> {
        // XXX FIXME: this is obviously wrong, but should work for
        // extension-storage / engines that don't do deduping. (Is it correct
        // that ext-storage won't want to dedupe on anything?)
        Ok(false)
    }

    /// Have we seen a schema with a required_version above ours? If we have, we
    /// only sync metadata until we get unstuck.
    pub(crate) fn in_sync_lockout(&self) -> Result<bool> {
        let stored = meta::try_get::<String>(self.conn(), meta::SYNC_NATIVE_VERSION_THRESHOLD)?;
        if let Some(v) = stored {
            let ver = match semver::VersionReq::parse(&v) {
                Ok(v) => v,
                Err(e) => {
                    log::error!(
                        "Illegal semver in {:?}: {}",
                        meta::SYNC_NATIVE_VERSION_THRESHOLD.0,
                        e
                    );
                    // Discard it -- it's just to avoid a bunch of expensive and pointless work.
                    meta::delete(self.conn(), meta::SYNC_NATIVE_VERSION_THRESHOLD)?;
                    return Ok(false);
                }
            };
            Ok(!ver.matches(&self.info.native_schema().version))
        } else {
            Ok(false)
        }
    }

    pub fn new_interrupt_handle(&self) -> SqlInterruptHandle {
        SqlInterruptHandle::new(
            self.db.get_interrupt_handle(),
            self.interrupt_counter.clone(),
        )
    }

    /// TODO: this function should return info about additional changes that
    /// need to be made.
    pub(crate) fn upgrade_remote(&mut self, action: &UpgradeRemote) -> Result<()> {
        let target = &self.info().local;
        if action.fresh_server {
            return Ok(());
        }
        let source = if let Some(v) = &action.from {
            v
        } else {
            return Ok(());
        };
        let compare = UpgradeKind::between(source, target);
        if compare == UpgradeKind::RequiresDedupe {
            // How to do this is described in the RFC, just needs impl.
            throw!(ErrorKind::NotYetImplemented(
                "Upgrades that add additional items to dedupe_on".to_string()
            ));
        }
        Ok(())
    }

    pub(crate) fn upgrade_local(&mut self, new_local: Arc<RecordSchema>) -> Result<()> {
        let compare = UpgradeKind::between(&self.info().local, &new_local);
        if compare == UpgradeKind::RequiresDedupe {
            // How to do this is described in the RFC, just needs impl.
            throw!(ErrorKind::NotYetImplemented(
                "Upgrades that add additional items to dedupe_on".to_string()
            ));
        }
        let tx = self.db.unchecked_transaction()?;
        // TODO: Need to make sure `new_local` doesn't reuse the `native` schema's ID.
        let sql = "
            REPLACE INTO remerge_schemas (is_legacy, version, required_version, schema_text)
            VALUES (:legacy, :version, :req_version, :text)
        ";
        let ver_str = new_local.version.to_string();
        self.db.execute_named(
            sql,
            rusqlite::named_params! {
                ":legacy": new_local.legacy,
                ":version": ver_str,
                ":req_version": new_local.required_version.to_string(),
                ":text": &*new_local.source,
            },
        )?;
        meta::put(&self.db, meta::LOCAL_SCHEMA_VERSION, &ver_str)?;
        tx.commit()?;
        self.info.local = new_local;
        Ok(())
    }

    #[inline]
    pub fn begin_interrupt_scope(&self) -> SqlInterruptScope {
        SqlInterruptScope::new(self.interrupt_counter.clone())
    }
}