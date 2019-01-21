use actix::prelude::*;

// This is really only used for long lived, high level types that need clone
// that otherwise can't be cloned. Think Mutex.
use std::sync::Arc;

use audit::AuditScope;
use be::{
    Backend, BackendError, BackendReadTransaction, BackendTransaction, BackendWriteTransaction,
};

use constants::{JSON_ANONYMOUS_V1, JSON_SYSTEM_INFO_V1};
use entry::Entry;
use error::OperationError;
use event::{CreateEvent, ExistsEvent, OpResult, SearchEvent, SearchResult};
use filter::Filter;
use log::EventLog;
use plugins::Plugins;
use schema::{Schema, SchemaTransaction, SchemaWriteTransaction};

pub fn start(log: actix::Addr<EventLog>, path: &str, threads: usize) -> actix::Addr<QueryServer> {
    let mut audit = AuditScope::new("server_start");
    let log_inner = log.clone();

    let qs_addr = audit_segment!(audit, || {
        // Create "just enough" schema for us to be able to load from
        // disk ... Schema loading is one time where we validate the
        // entries as we read them, so we need this here.
        // FIXME: Handle results in start correctly
        let schema = Arc::new(Schema::new(&mut audit).unwrap());
        let mut audit_be = AuditScope::new("backend_new");
        let be = Backend::new(&mut audit_be, path).unwrap();
        {
            // Create a new backend audit scope
            let be_txn = be.write();
            let mut schema_write = schema.write();
            audit.append_scope(audit_be);

            // Now, we have the initial schema in memory. Use this to trigger
            // an index of the be for the core schema.

            // Now search for the schema itself, and validate that the system
            // in memory matches the BE on disk, and that it's syntactically correct.
            // Write it out if changes are needed.

            // Now load the remaining backend schema into memory.
            // TODO: Schema elements should be versioned individually.
            schema_write.bootstrap_core(&mut audit).unwrap();

            // TODO: Backend setup indexes as needed from schema, for the core
            // system schema.
            // TODO: Trigger an index? This could be costly ...
            //   Perhaps a config option to say if we index on startup or not.
            // TODO: Check the results!
            schema_write.validate(&mut audit);
            be_txn.commit();
            schema_write.commit();
        }

        // Create a temporary query_server implementation
        let query_server = QueryServer::new(log_inner.clone(), be.clone(), schema.clone());

        let mut audit_qsc = AuditScope::new("query_server_init");
        let query_server_write = query_server.write();
        query_server_write.initialise(&mut audit_qsc);
        // We are good to go! Finally commit and consume the txn.
        audit_segment!(audit_qsc, || query_server_write.commit(&mut audit_qsc));
        audit.append_scope(audit_qsc);

        SyncArbiter::start(threads, move || {
            QueryServer::new(log_inner.clone(), be.clone(), schema.clone())
        })
    });
    log.do_send(audit);
    qs_addr
}

// This is the core of the server. It implements all
// the search and modify actions, applies access controls
// and get's everything ready to push back to the fe code

// This is it's own actor, so we can have a write addr and a read addr,
// and it allows serialisation that way rather than relying on
// the backend
pub trait QueryServerReadTransaction {
    type BackendTransactionType: BackendReadTransaction;

    fn get_be_txn(&self) -> &Self::BackendTransactionType;

    fn search(&self, au: &mut AuditScope, se: &SearchEvent) -> Result<Vec<Entry>, OperationError> {
        // TODO: Validate the filter
        // This is an important security step because it prevents us from
        // performing un-indexed searches on attr's that don't exist in the
        // server. This is why ExtensibleObject can only take schema that
        // exists in the server, not arbitrary attr names.

        // TODO: Normalise the filter

        // TODO: Assert access control allows the filter and requested attrs.

        // TODO: Pre-search plugins

        let mut audit_be = AuditScope::new("backend_search");
        let res = self
            .get_be_txn()
            .search(&mut audit_be, &se.filter)
            .map(|r| r)
            .map_err(|_| OperationError::Backend);
        au.append_scope(audit_be);

        // TODO: Post-search plugins

        // TODO: We'll add ACI here. I think ACI should transform from
        // internal -> proto entries since we have to anyway ...
        // alternately, we can just clone again ...
        res
    }

    fn exists(&self, au: &mut AuditScope, ee: &ExistsEvent) -> Result<bool, OperationError> {
        let mut audit_be = AuditScope::new("backend_exists");
        let res = self
            .get_be_txn()
            .exists(&mut audit_be, &ee.filter)
            .map(|r| r)
            .map_err(|_| OperationError::Backend);
        au.append_scope(audit_be);
        res
    }

    // From internal, generate an exists event and dispatch
    fn internal_exists(&self, au: &mut AuditScope, filter: Filter) -> Result<bool, OperationError> {
        let mut audit_int = AuditScope::new("internal_exists");
        // Build an exists event
        let ee = ExistsEvent::new_internal(filter);
        // Submit it
        let res = self.exists(&mut audit_int, &ee);
        au.append_scope(audit_int);
        // return result
        res
    }

    fn internal_search(&self, _au: &mut AuditScope, _filter: Filter) -> Result<(), ()> {
        unimplemented!()
    }
}

pub struct QueryServerTransaction {
    be_txn: BackendTransaction,
    // Anything else? In the future, we'll need to have a schema transaction
    // type, maybe others?
    schema: SchemaTransaction,
}

// Actually conduct a search request
// This is the core of the server, as it processes the entire event
// applies all parts required in order and more.
impl QueryServerReadTransaction for QueryServerTransaction {
    type BackendTransactionType = BackendTransaction;

    fn get_be_txn(&self) -> &BackendTransaction {
        &self.be_txn
    }
}

pub struct QueryServerWriteTransaction<'a> {
    committed: bool,
    // be_write_txn: BackendWriteTransaction,
    // schema_write: SchemaWriteTransaction,
    // read: QueryServerTransaction,
    be_txn: BackendWriteTransaction,
    schema: SchemaWriteTransaction<'a>,
}

impl<'a> QueryServerReadTransaction for QueryServerWriteTransaction<'a> {
    type BackendTransactionType = BackendWriteTransaction;

    fn get_be_txn(&self) -> &BackendWriteTransaction {
        &self.be_txn
    }
}

pub struct QueryServer {
    log: actix::Addr<EventLog>,
    // be: actix::Addr<BackendActor>,
    // This probably needs to be Arc, or a ref. How do we want to manage this?
    // I think the BE is build, configured and cloned? Maybe Backend
    // is a wrapper type to Arc<BackendInner> or something.
    be: Backend,
    schema: Arc<Schema>,
}

impl QueryServer {
    pub fn new(log: actix::Addr<EventLog>, be: Backend, schema: Arc<Schema>) -> Self {
        log_event!(log, "Starting query worker ...");
        QueryServer {
            log: log,
            be: be,
            schema: schema,
        }
    }

    pub fn read(&self) -> QueryServerTransaction {
        QueryServerTransaction {
            be_txn: self.be.read(),
            schema: self.schema.read(),
        }
    }

    pub fn write(&self) -> QueryServerWriteTransaction {
        QueryServerWriteTransaction {
            // I think this is *not* needed, because commit is mut self which should
            // take ownership of the value, and cause the commit to "only be run
            // once".
            //
            // The commited flag is however used for abort-specific code in drop
            // which today I don't think we have ... yet.
            committed: false,
            be_txn: self.be.write(),
            schema: self.schema.write(),
        }
    }
}

impl<'a> QueryServerWriteTransaction<'a> {
    pub fn create(&self, au: &mut AuditScope, ce: &CreateEvent) -> Result<(), OperationError> {
        // The create event is a raw, read only representation of the request
        // that was made to us, including information about the identity
        // performing the request.

        // Log the request

        // TODO: Do we need limits on number of creates, or do we constraint
        // based on request size in the frontend?

        // Copy the entries to a writeable form.
        let mut candidates: Vec<Entry> = ce.entries.iter().map(|er| er.clone()).collect();

        // run any pre plugins, giving them the list of mutable candidates.
        // pre-plugins are defined here in their correct order of calling!
        // I have no intent to make these dynamic or configurable.

        let mut audit_plugin_pre = AuditScope::new("plugin_pre_create");
        let plug_pre_res = Plugins::run_pre_create(
            &self.be_txn,
            &mut audit_plugin_pre,
            &mut candidates,
            ce,
            &self.schema,
        );
        au.append_scope(audit_plugin_pre);

        if plug_pre_res.is_err() {
            audit_log!(au, "Create operation failed (plugin), {:?}", plug_pre_res);
            return plug_pre_res;
        }

        let r = candidates.iter().fold(Ok(()), |acc, e| {
            if acc.is_ok() {
                self.schema
                    .validate_entry(e)
                    .map_err(|_| OperationError::SchemaViolation)
            } else {
                acc
            }
        });
        if r.is_err() {
            audit_log!(au, "Create operation failed (schema), {:?}", r);
            return r;
        }

        // Normalise all the data now it's validated.
        // FIXME: This normalisation COPIES everything, which may be
        // slow.
        let norm_cand: Vec<Entry> = candidates
            .iter()
            .map(|e| self.schema.normalise_entry(&e))
            .collect();

        let mut audit_be = AuditScope::new("backend_create");
        // We may change from ce.entries later to something else?
        let res = self
            .be_txn
            .create(&mut audit_be, &norm_cand)
            .map(|_| ())
            .map_err(|e| match e {
                BackendError::EmptyRequest => OperationError::EmptyRequest,
            });
        au.append_scope(audit_be);

        if res.is_err() {
            // be_txn is dropped, ie aborted here.
            audit_log!(au, "Create operation failed (backend), {:?}", r);
            return res;
        }
        // Run any post plugins

        // Commit the txn
        // let commit, commit!
        // be_txn.commit();

        // We are complete, finalise logging and return

        audit_log!(au, "Create operation success");
        res
    }

    // internal server operation types.
    // These just wrap the fn create/search etc, but they allow
    // creating the needed create event with the correct internal flags
    // and markers. They act as though they have the highest level privilege
    // IE there are no access control checks.

    pub fn internal_exists_or_create(&self, e: Entry) -> Result<(), OperationError> {
        // If the thing exists, stop.
        // if not, create from Entry.
        unimplemented!()
    }

    pub fn internal_migrate_or_create(
        &self,
        audit: &mut AuditScope,
        e: Entry,
    ) -> Result<(), OperationError> {
        // if the thing exists, ensure the set of attributes on
        // Entry A match and are present (but don't delete multivalue, or extended
        // attributes in the situation.
        // If not exist, create from Entry B
        //
        // WARNING: this requires schema awareness for multivalue types!
        // We need to either do a schema aware merge, or we just overwrite those
        // few attributes.
        //
        // This will extra classes an attributes alone!
        let filt = match e.filter_from_attrs(&vec![String::from("name"), String::from("uuid")]) {
            Some(f) => f,
            None => return Err(OperationError::FilterGeneration),
        };

        // Does it exist? (TODO: Should be search, not exists ...)
        match self.internal_exists(audit, filt) {
            Ok(true) => {
                // it exists. We need to ensure the content now.
                unimplemented!()
            }
            Ok(false) => {
                // It does not exist. Create it.
                self.internal_create(audit, vec![e])
            }
            Err(e) => {
                // An error occured. pass it back up.
                Err(e)
            }
        }
    }

    // Should this take a be_txn?
    pub fn internal_assert_or_create(
        &self,
        audit: &mut AuditScope,
        e: Entry,
    ) -> Result<(), OperationError> {
        // If exists, ensure the object is exactly as provided
        // else, if not exists, create it. IE no extra or excess
        // attributes and classes.

        // Create a filter from the entry for assertion.
        let filt = match e.filter_from_attrs(&vec![String::from("name"), String::from("uuid")]) {
            Some(f) => f,
            None => return Err(OperationError::FilterGeneration),
        };

        // Does it exist? (TODO: Should be search, not exists ...)
        match self.internal_exists(audit, filt) {
            Ok(true) => {
                // it exists. We need to ensure the content now.
                unimplemented!()
            }
            Ok(false) => {
                // It does not exist. Create it.
                self.internal_create(audit, vec![e])
            }
            Err(e) => {
                // An error occured. pass it back up.
                Err(e)
            }
        }
    }

    // These are where searches and other actions are actually implemented. This
    // is the "internal" version, where we define the event as being internal
    // only, allowing certain plugin by passes etc.

    pub fn internal_create(
        &self,
        audit: &mut AuditScope,
        entries: Vec<Entry>,
    ) -> Result<(), OperationError> {
        // Start the audit scope
        let mut audit_int = AuditScope::new("internal_create");
        // Create the CreateEvent
        let ce = CreateEvent::new_internal(entries);
        let res = self.create(&mut audit_int, &ce);
        audit.append_scope(audit_int);
        res
    }

    // This function is idempotent
    pub fn initialise(&self, audit: &mut AuditScope) -> Result<(), OperationError> {
        // First, check the system_info object. This stores some server information
        // and details. It's a pretty static thing.
        let mut audit_si = AuditScope::new("start_system_info");
        let res = audit_segment!(audit_si, || {
            let e: Entry = serde_json::from_str(JSON_SYSTEM_INFO_V1).unwrap();
            self.internal_assert_or_create(audit, e)
        });
        audit.append_scope(audit_si);
        if res.is_err() {
            return res;
        }

        // Check the anonymous object exists (migrations).
        let mut audit_an = AuditScope::new("start_anonymous");
        let res = audit_segment!(audit_an, || {
            let e: Entry = serde_json::from_str(JSON_ANONYMOUS_V1).unwrap();
            self.internal_migrate_or_create(audit, e)
        });
        audit.append_scope(audit_an);
        if res.is_err() {
            return res;
        }

        // Check the admin object exists (migrations).

        // Load access profiles and configure them.
        Ok(())
    }

    pub fn commit(self, audit: &mut AuditScope) -> Result<(), ()> {
        let QueryServerWriteTransaction {
            committed,
            be_txn,
            schema,
        } = self;
        assert!(!committed);
        // Begin an audit.
        // Validate the schema,
        schema
            .validate(audit)
            // TODO: At this point, if validate passes, we probably actually want
            // to perform a reload BEFORE we commit.
            // Alternate, we attempt to reload during batch ops, but this seems
            // costly.
            .map(|_| {
                // Backend Commit
                be_txn.commit()
            })
            .map(|_| {
                // Schema commit: Since validate passed and be is good, this
                // must now also be good.
                schema.commit()
            })
        // Audit done
    }
}

impl Actor for QueryServer {
    type Context = SyncContext<Self>;

    /*
    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.set_mailbox_capacity(1 << 31);
    }
    */
}

// The server only recieves "Event" structures, which
// are whole self contained DB operations with all parsing
// required complete. We still need to do certain validation steps, but
// at this point our just is just to route to do_<action>

impl Handler<SearchEvent> for QueryServer {
    type Result = Result<SearchResult, OperationError>;

    fn handle(&mut self, msg: SearchEvent, _: &mut Self::Context) -> Self::Result {
        let mut audit = AuditScope::new("search");
        let res = audit_segment!(&mut audit, || {
            audit_log!(audit, "Begin event {:?}", msg);
            // Begin a read
            let qs_read = self.read();

            // Parse what we need from the event?
            // What kind of event is it?

            // In the future we'll likely change search event ...

            // End the read

            // was this ok?
            match qs_read.search(&mut audit, &msg) {
                Ok(entries) => Ok(SearchResult::new(entries)),
                Err(e) => Err(e),
            }
        });
        // At the end of the event we send it for logging.
        self.log.do_send(audit);
        res
    }
}

impl Handler<CreateEvent> for QueryServer {
    type Result = Result<OpResult, OperationError>;

    fn handle(&mut self, msg: CreateEvent, _: &mut Self::Context) -> Self::Result {
        let mut audit = AuditScope::new("create");
        let res = audit_segment!(&mut audit, || {
            audit_log!(audit, "Begin create event {:?}", msg);

            let qs_write = self.write();

            match qs_write.create(&mut audit, &msg) {
                Ok(()) => {
                    qs_write.commit(&mut audit);
                    Ok(OpResult {})
                }
                Err(e) => Err(e),
            }
        });
        // At the end of the event we send it for logging.
        self.log.do_send(audit);
        res
    }
}

// Auth requests? How do we structure these ...

#[cfg(test)]
mod tests {
    extern crate actix;
    use actix::prelude::*;

    extern crate futures;
    use futures::future;
    use futures::future::Future;
    use std::sync::Arc;

    extern crate tokio;

    use super::super::audit::AuditScope;
    use super::super::be::{Backend, BackendTransaction};
    use super::super::entry::Entry;
    use super::super::event::{CreateEvent, SearchEvent};
    use super::super::filter::Filter;
    use super::super::log;
    use super::super::proto_v1::Entry as ProtoEntry;
    use super::super::proto_v1::{CreateRequest, SearchRequest};
    use super::super::schema::Schema;
    use super::super::server::{
        QueryServer, QueryServerReadTransaction, QueryServerWriteTransaction,
    };

    macro_rules! run_test {
        ($test_fn:expr) => {{
            System::run(|| {
                let mut audit = AuditScope::new("run_test");
                let test_log = log::start();

                let be = Backend::new(&mut audit, "").unwrap();
                let mut schema_outer = Schema::new(&mut audit).unwrap();
                {
                    let mut schema = schema_outer.write();
                    schema.bootstrap_core(&mut audit).unwrap();
                    schema.commit();
                }
                let test_server = QueryServer::new(test_log.clone(), be, Arc::new(schema_outer));

                // Could wrap another future here for the future::ok bit...
                let fut = $test_fn(test_log.clone(), test_server, &mut audit);
                let comp_fut = fut.map_err(|()| ()).and_then(move |_r| {
                    test_log.do_send(audit);
                    println!("Stopping actix ...");
                    actix::System::current().stop();
                    future::result(Ok(()))
                });

                tokio::spawn(comp_fut);
            });
        }};
    }

    #[test]
    fn test_be_create_user() {
        run_test!(|_log, mut server: QueryServer, audit: &mut AuditScope| {
            let mut server_txn = server.write();
            let filt = Filter::Pres(String::from("name"));

            let se1 = SearchEvent::from_request(SearchRequest::new(filt.clone()));
            let se2 = SearchEvent::from_request(SearchRequest::new(filt));

            let e: Entry = serde_json::from_str(
                r#"{
                "attrs": {
                    "class": ["object", "person"],
                    "name": ["testperson"],
                    "uuid": ["cc8e95b4-c24f-4d68-ba54-8bed76f63930"],
                    "description": ["testperson"],
                    "displayname": ["testperson"]
                }
            }"#,
            )
            .unwrap();

            let expected = vec![e];

            let ce = CreateEvent::from_vec(expected.clone());

            let r1 = server_txn.search(audit, &se1).unwrap();
            assert!(r1.len() == 0);

            let cr = server_txn.create(audit, &ce);
            assert!(cr.is_ok());

            let r2 = server_txn.search(audit, &se2).unwrap();
            println!("--> {:?}", r2);
            assert!(r2.len() == 1);

            assert_eq!(r2, expected);

            assert!(server_txn.commit(audit).is_ok());

            future::ok(())
        });
    }

    // Test Create Empty

    // Test Init is Idempotent

    #[test]
    fn test_qs_init_idempotent_1() {
        run_test!(|_log, mut server: QueryServer, audit: &mut AuditScope| {
            {
                // Setup and abort.
                let server_txn = server.write();
                assert!(server_txn.initialise(audit).is_ok());
            }
            {
                let server_txn = server.write();
                assert!(server_txn.initialise(audit).is_ok());
                assert!(server_txn.initialise(audit).is_ok());
                assert!(server_txn.commit(audit).is_ok());
            }
            {
                // Now do it again in a new txn, but abort
                let server_txn = server.write();
                assert!(server_txn.initialise(audit).is_ok());
            }
            {
                // Now do it again in a new txn.
                let server_txn = server.write();
                assert!(server_txn.initialise(audit).is_ok());
                assert!(server_txn.commit(audit).is_ok());
            }

            future::ok(())
        });
    }
}