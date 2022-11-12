//! This plugin maintains consistency of authenticated sessions on accounts.
//!
//! An example of this is that oauth2 sessions are child of user auth sessions,
//! such than when the user auth session is terminated, then the corresponding
//! oauth2 session should also be terminated.
//!
//! This plugin is also responsible for invaliding old sessions that are past
//! their expiry.

use crate::event::ModifyEvent;
use crate::plugins::Plugin;
use crate::prelude::*;

pub struct SessionConsistency {}

impl Plugin for SessionConsistency {
    fn id() -> &'static str {
        "plugin_session_consistency"
    }

    #[instrument(level = "debug", name = "session_consistency", skip_all)]
    fn pre_modify(
        qs: &mut QueryServerWriteTransaction,
        _cand: &mut Vec<Entry<EntryInvalid, EntryCommitted>>,
        _me: &ModifyEvent,
    ) -> Result<(), OperationError> {
        let _curtime = qs.get_curtime();
        /*
        cand.iter_mut().try_for_each(|_e| {
        });
        */

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // use kanidm_proto::v1::PluginError;
    use crate::prelude::*;

    use crate::event::CreateEvent;
    use crate::value::{Oauth2Session, Session};
    use std::time::Duration;
    use time::OffsetDateTime;
    use uuid::uuid;

    // Test expiry of old sessions

    #[qs_test]
    async fn test_session_consistency_expire_old_sessions(server: &QueryServer) {
        let curtime = duration_from_epoch_now();
        let curtime_odt = OffsetDateTime::unix_epoch() + curtime;

        let exp_curtime = curtime + Duration::from_secs(60);
        let exp_curtime_odt = OffsetDateTime::unix_epoch() + exp_curtime;

        // Create a user
        let mut server_txn = server.write(curtime).await;

        let tuuid = uuid!("cc8e95b4-c24f-4d68-ba54-8bed76f63930");

        let e1 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("person")),
            ("class", Value::new_class("account")),
            ("name", Value::new_iname("testperson1")),
            ("uuid", Value::new_uuid(tuuid)),
            ("description", Value::new_utf8s("testperson1")),
            ("displayname", Value::new_utf8s("testperson1"))
        );

        let ce = CreateEvent::new_internal(vec![e1]);
        assert!(server_txn.create(&ce).is_ok());

        // Create a fake session.
        let session_id = Uuid::new_v4();
        let pv_session_id = PartialValue::new_refer(session_id);
        let expiry = Some(exp_curtime_odt);
        let issued_at = curtime_odt;
        let issued_by = IdentityId::User(tuuid);
        let scope = AccessScope::IdentityOnly;

        let session = Value::Session(
            session_id,
            Session {
                label: "label".to_string(),
                expiry,
                // Need the other inner bits?
                // for the gracewindow.
                issued_at,
                // Who actually created this?
                issued_by,
                // What is the access scope of this session? This is
                // for auditing purposes.
                scope,
            },
        );

        // Mod the user
        let modlist = ModifyList::new_append("user_auth_token_session", session);

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Still there

        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        assert!(entry.attribute_equality("user_auth_token_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());
        let mut server_txn = server.write(exp_curtime).await;

        // Mod again - anything will do.
        let modlist =
            ModifyList::new_purge_and_set("description", Value::new_utf8s("test person 1 change"));

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Session gone.
        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        // Note it's a not condition now.
        assert!(!entry.attribute_equality("user_auth_token_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());
    }

    // Test expiry of old oauth2 sessions
    #[qs_test]
    async fn test_session_consistency_oauth2_expiry_cleanup(server: &QueryServer) {
        let curtime = duration_from_epoch_now();
        let curtime_odt = OffsetDateTime::unix_epoch() + curtime;

        // Set exp to gracewindow.
        let exp_curtime = curtime + GRACE_WINDOW;
        let exp_curtime_odt = OffsetDateTime::unix_epoch() + exp_curtime;

        // Create a user
        let mut server_txn = server.write(curtime).await;

        let tuuid = uuid!("cc8e95b4-c24f-4d68-ba54-8bed76f63930");
        let rs_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("person")),
            ("class", Value::new_class("account")),
            ("name", Value::new_iname("testperson1")),
            ("uuid", Value::new_uuid(tuuid)),
            ("description", Value::new_utf8s("testperson1")),
            ("displayname", Value::new_utf8s("testperson1"))
        );

        let e2 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("oauth2_resource_server")),
            ("class", Value::new_class("oauth2_resource_server_basic")),
            ("uuid", Value::new_uuid(rs_uuid)),
            ("oauth2_rs_name", Value::new_iname("test_resource_server")),
            ("displayname", Value::new_utf8s("test_resource_server")),
            (
                "oauth2_rs_origin",
                Value::new_url_s("https://demo.example.com").unwrap()
            ),
            // System admins
            (
                "oauth2_rs_scope_map",
                Value::new_oauthscopemap(UUID_IDM_ALL_ACCOUNTS, btreeset!["openid".to_string()])
                    .expect("invalid oauthscope")
            )
        );

        let ce = CreateEvent::new_internal(vec![e1, e2]);
        assert!(server_txn.create(&ce).is_ok());

        // Create a fake session and oauth2 session.

        let session_id = Uuid::new_v4();
        let pv_session_id = PartialValue::new_refer(session_id);

        let parent = Uuid::new_v4();
        let pv_parent_id = PartialValue::new_refer(parent);
        let expiry = Some(exp_curtime_odt);
        let issued_at = curtime_odt;
        let issued_by = IdentityId::User(tuuid);
        let scope = AccessScope::IdentityOnly;

        // Mod the user
        let modlist = modlist!([
            Modify::Present(
                "oauth2_session".into(),
                Value::Oauth2Session(
                    session_id,
                    Oauth2Session {
                        parent,
                        // Set to the exp window.
                        expiry,
                        issued_at,
                        rs_uuid,
                    },
                )
            ),
            Modify::Present(
                "user_auth_token_session".into(),
                Value::Session(
                    parent,
                    Session {
                        label: "label".to_string(),
                        // Note we set the exp to None so we are not removing based on removal of the parent.
                        expiry: None,
                        // Need the other inner bits?
                        // for the gracewindow.
                        issued_at,
                        // Who actually created this?
                        issued_by,
                        // What is the access scope of this session? This is
                        // for auditing purposes.
                        scope,
                    },
                )
            ),
        ]);

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Still there

        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        assert!(entry.attribute_equality("user_auth_token_session", &pv_parent_id));
        assert!(entry.attribute_equality("oauth2_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());

        // Note as we are now past exp time, the oauth2 session will be removed, but the uat session
        // will remain.
        let mut server_txn = server.write(exp_curtime).await;

        // Mod again - anything will do.
        let modlist =
            ModifyList::new_purge_and_set("description", Value::new_utf8s("test person 1 change"));

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Session gone.
        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        // Note the uat is still present
        assert!(entry.attribute_equality("user_auth_token_session", &pv_parent_id));
        // Note it's a not condition now.
        assert!(!entry.attribute_equality("oauth2_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());
    }

    // test removal of a session removes related oauth2 sessions.
    #[qs_test]
    async fn test_session_consistency_oauth2_removed_by_parent(server: &QueryServer) {
        let curtime = duration_from_epoch_now();
        let curtime_odt = OffsetDateTime::unix_epoch() + curtime;

        // Create a user
        let mut server_txn = server.write(curtime).await;

        let tuuid = uuid!("cc8e95b4-c24f-4d68-ba54-8bed76f63930");
        let rs_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("person")),
            ("class", Value::new_class("account")),
            ("name", Value::new_iname("testperson1")),
            ("uuid", Value::new_uuid(tuuid)),
            ("description", Value::new_utf8s("testperson1")),
            ("displayname", Value::new_utf8s("testperson1"))
        );

        let e2 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("oauth2_resource_server")),
            ("class", Value::new_class("oauth2_resource_server_basic")),
            ("uuid", Value::new_uuid(rs_uuid)),
            ("oauth2_rs_name", Value::new_iname("test_resource_server")),
            ("displayname", Value::new_utf8s("test_resource_server")),
            (
                "oauth2_rs_origin",
                Value::new_url_s("https://demo.example.com").unwrap()
            ),
            // System admins
            (
                "oauth2_rs_scope_map",
                Value::new_oauthscopemap(UUID_IDM_ALL_ACCOUNTS, btreeset!["openid".to_string()])
                    .expect("invalid oauthscope")
            )
        );

        let ce = CreateEvent::new_internal(vec![e1, e2]);
        assert!(server_txn.create(&ce).is_ok());

        // Create a fake session and oauth2 session.

        let session_id = Uuid::new_v4();
        let pv_session_id = PartialValue::new_refer(session_id);

        let parent = Uuid::new_v4();
        let pv_parent_id = PartialValue::new_refer(parent);
        let issued_at = curtime_odt;
        let issued_by = IdentityId::User(tuuid);
        let scope = AccessScope::IdentityOnly;

        // Mod the user
        let modlist = modlist!([
            Modify::Present(
                "oauth2_session".into(),
                Value::Oauth2Session(
                    session_id,
                    Oauth2Session {
                        parent,
                        // Note we set the exp to None so we are not removing based on exp
                        expiry: None,
                        issued_at,
                        rs_uuid,
                    },
                )
            ),
            Modify::Present(
                "user_auth_token_session".into(),
                Value::Session(
                    parent,
                    Session {
                        label: "label".to_string(),
                        // Note we set the exp to None so we are not removing based on removal of the parent.
                        expiry: None,
                        // Need the other inner bits?
                        // for the gracewindow.
                        issued_at,
                        // Who actually created this?
                        issued_by,
                        // What is the access scope of this session? This is
                        // for auditing purposes.
                        scope,
                    },
                )
            ),
        ]);

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Still there

        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        assert!(entry.attribute_equality("user_auth_token_session", &pv_parent_id));
        assert!(entry.attribute_equality("oauth2_session", &pv_session_id));

        // Mod again - remove the parent session.
        let modlist = ModifyList::new_remove("user_auth_token_session", pv_parent_id.clone());

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Session gone.
        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        // Note the uat is removed
        assert!(!entry.attribute_equality("user_auth_token_session", &pv_parent_id));
        // The oauth2 session is also removed.
        assert!(!entry.attribute_equality("oauth2_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());
    }

    // Test if an oauth2 session exists, the grace window passes and it's UAT doesn't exist.
    #[qs_test]
    async fn test_session_consistency_oauth2_grace_window_past(server: &QueryServer) {
        let curtime = duration_from_epoch_now();
        let curtime_odt = OffsetDateTime::unix_epoch() + curtime;

        // Set exp to gracewindow.
        let exp_curtime = curtime + GRACE_WINDOW;
        // let exp_curtime_odt = OffsetDateTime::unix_epoch() + exp_curtime;

        // Create a user
        let mut server_txn = server.write(curtime).await;

        let tuuid = uuid!("cc8e95b4-c24f-4d68-ba54-8bed76f63930");
        let rs_uuid = Uuid::new_v4();

        let e1 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("person")),
            ("class", Value::new_class("account")),
            ("name", Value::new_iname("testperson1")),
            ("uuid", Value::new_uuid(tuuid)),
            ("description", Value::new_utf8s("testperson1")),
            ("displayname", Value::new_utf8s("testperson1"))
        );

        let e2 = entry_init!(
            ("class", Value::new_class("object")),
            ("class", Value::new_class("oauth2_resource_server")),
            ("class", Value::new_class("oauth2_resource_server_basic")),
            ("uuid", Value::new_uuid(rs_uuid)),
            ("oauth2_rs_name", Value::new_iname("test_resource_server")),
            ("displayname", Value::new_utf8s("test_resource_server")),
            (
                "oauth2_rs_origin",
                Value::new_url_s("https://demo.example.com").unwrap()
            ),
            // System admins
            (
                "oauth2_rs_scope_map",
                Value::new_oauthscopemap(UUID_IDM_ALL_ACCOUNTS, btreeset!["openid".to_string()])
                    .expect("invalid oauthscope")
            )
        );

        let ce = CreateEvent::new_internal(vec![e1, e2]);
        assert!(server_txn.create(&ce).is_ok());

        // Create a fake session.
        let session_id = Uuid::new_v4();
        let pv_session_id = PartialValue::new_refer(session_id);

        let parent = Uuid::new_v4();
        let issued_at = curtime_odt;

        let session = Value::Oauth2Session(
            session_id,
            Oauth2Session {
                parent,
                // Note we set the exp to None so we are asserting the removal is due to the lack
                // of the parent session.
                expiry: None,
                issued_at,
                rs_uuid,
            },
        );

        // Mod the user
        let modlist = ModifyList::new_append("oauth2_session", session);

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Still there

        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        assert!(entry.attribute_equality("oauth2_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());

        // Note the exp_curtime now is past the gracewindow. This will trigger
        // consistency to purge the un-matched session.
        let mut server_txn = server.write(exp_curtime).await;

        // Mod again - anything will do.
        let modlist =
            ModifyList::new_purge_and_set("description", Value::new_utf8s("test person 1 change"));

        server_txn
            .internal_modify(
                &filter!(f_eq("uuid", PartialValue::new_uuid(tuuid))),
                &modlist,
            )
            .expect("Failed to modify user");

        // Session gone.
        let entry = server_txn.internal_search_uuid(&tuuid).expect("failed");

        // Note it's a not condition now.
        assert!(!entry.attribute_equality("oauth2_session", &pv_session_id));

        assert!(server_txn.commit().is_ok());
    }
}
