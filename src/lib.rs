//
// Copyright (c) 2017, 2020 ADLINK Technology Inc.
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ADLINK zenoh team, <zenoh@adlink-labs.tech>
//

use async_std::task;
use async_trait::async_trait;
use influxdb::{
    Client, Query as InfluxQuery, Timestamp as InfluxTimestamp, WriteQuery as InfluxWQuery,
};
use log::{debug, error, warn};
use regex::Regex;
use serde::Deserialize;
use std::borrow::Cow;
use std::convert::TryFrom;
use std::str::FromStr;
use std::time::{Duration, Instant};
use uuid::Uuid;
use zenoh::net::{DataInfo, Sample};
use zenoh::{
    Change, ChangeKind, Properties, Selector, Timestamp, Value, ZError, ZErrorKind, ZResult,
};
use zenoh_backend_traits::*;
use zenoh_util::collections::{Timed, TimedEvent, TimedHandle, Timer};
use zenoh_util::{zerror, zerror2};

// Properies used by the Backend
pub const PROP_BACKEND_URL: &str = "url";
pub const PROP_BACKEND_USERNAME: &str = "username";
pub const PROP_BACKEND_PASSWORD: &str = "password";

// Properies used by the Storage
pub const PROP_STORAGE_DB: &str = "db";
pub const PROP_STORAGE_CREATE_DB: &str = "create_db";
pub const PROP_STORAGE_ON_CLOSURE: &str = "on_closure";
pub const PROP_STORAGE_USERNAME: &str = PROP_BACKEND_USERNAME;
pub const PROP_STORAGE_PASSWORD: &str = PROP_BACKEND_PASSWORD;

// delay after deletion to drop a measurement
const DROP_MEASUREMENT_TIMEOUT_MS: u64 = 5000;

const GIT_VERSION: &str = git_version::git_version!(prefix = "v", cargo_prefix = "v");
lazy_static::lazy_static!(
    static ref LONG_VERSION: String = format!("{} built with {}", GIT_VERSION, env!("RUSTC_VERSION"));
);

#[no_mangle]
pub fn create_backend(properties: &Properties) -> ZResult<Box<dyn Backend>> {
    // For some reasons env_logger is sometime not active in a loaded library.
    // Try to activate it here, ignoring failures.
    let _ = env_logger::try_init();
    debug!("InfluxDB backend {}", LONG_VERSION.as_str());

    // work on a copy of properties to update them before re-use as admin_status.
    let mut props = properties.clone();
    props.insert("version".into(), LONG_VERSION.clone());

    let url = match props.get(PROP_BACKEND_URL) {
        Some(url) => url.clone(),
        None => {
            return zerror!(ZErrorKind::Other {
                descr: format!(
                    "Properties for InfluxDb Backend miss '{}'",
                    PROP_BACKEND_URL
                )
            })
        }
    };

    // The InfluxDB client used for administration purposes (show/create/drop databases)
    let mut admin_client = Client::new(&url, "");

    // Note: remove username/password from properties to not re-expose them in admin_status
    let credentials = match (
        props.remove(PROP_BACKEND_USERNAME),
        props.remove(PROP_BACKEND_PASSWORD),
    ) {
        (Some(username), Some(password)) => {
            admin_client = admin_client.with_auth(&username, &password);
            Some((username, password))
        }
        (None, None) => None,
        (None, _) => {
            return zerror!(ZErrorKind::Other {
                descr: format!(
                    "Properties for InfluxDb Backend includes '{}' but not '{}",
                    PROP_BACKEND_USERNAME, PROP_BACKEND_PASSWORD
                )
            })
        }
        (_, None) => {
            return zerror!(ZErrorKind::Other {
                descr: format!(
                    "Properties for InfluxDb Backend includes '{}' but not '{}",
                    PROP_BACKEND_PASSWORD, PROP_BACKEND_USERNAME
                )
            })
        }
    };

    // Check connectivity to InfluxDB, no need for a database for this
    let admin_client_copy = admin_client.clone();
    match async_std::task::block_on(async move { admin_client_copy.ping().await }) {
        Ok(_) => {
            props.insert(PROP_BACKEND_TYPE.into(), "InfluxDB".into());
            let admin_status = zenoh::utils::properties_to_json_value(&props);
            Ok(Box::new(InfluxDbBackend {
                admin_status,
                admin_client,
                credentials,
            }))
        }
        Err(err) => zerror!(ZErrorKind::Other {
            descr: format!("Failed to create InfluxDb Backend : {}", err)
        }),
    }
}

pub struct InfluxDbBackend {
    admin_status: Value,
    admin_client: Client,
    credentials: Option<(String, String)>,
}

#[async_trait]
impl Backend for InfluxDbBackend {
    async fn get_admin_status(&self) -> Value {
        self.admin_status.clone()
    }

    async fn create_storage(&mut self, properties: Properties) -> ZResult<Box<dyn Storage>> {
        // work on a copy of properties to update them before re-use as admin_status.
        let mut props = properties.clone();

        let path_expr = props.get(PROP_STORAGE_PATH_EXPR).unwrap();
        let path_prefix = match props.get(PROP_STORAGE_PATH_PREFIX) {
            Some(p) => {
                if !path_expr.starts_with(p) {
                    return zerror!(ZErrorKind::Other {
                        descr: format!(
                            "The specified {}={} is not a prefix of {}={}",
                            PROP_STORAGE_PATH_PREFIX, p, PROP_STORAGE_PATH_EXPR, path_expr
                        )
                    });
                }
                Some(p.to_string())
            }
            None => None,
        };
        let on_closure = OnClosure::try_from(&props)?;
        let (db, createdb) = match (
            props.get(PROP_STORAGE_DB),
            props.contains_key(PROP_STORAGE_CREATE_DB),
        ) {
            (Some(name), b) => (name.clone(), b),
            (None, _) => {
                let name = generate_db_name();
                // insert generated name in props to be re-exposed in admin_status
                props.insert(PROP_STORAGE_DB.to_string(), name.clone());
                // force DB creation, even if not explicitly specified
                (name, true)
            }
        };

        // The Influx client on database used to write/query on this storage
        // (using the same URL than backend's admin_client, but with storage credentials)
        let mut client = Client::new(self.admin_client.database_url(), &db);
        // Note: remove username/password from properties to not re-expose them in admin_status
        let storage_username = match (
            props.remove(PROP_STORAGE_USERNAME),
            props.remove(PROP_STORAGE_PASSWORD),
        ) {
            (Some(username), Some(password)) => {
                client = client.with_auth(&username, password);
                Some(username)
            }
            (None, None) => None,
            (None, _) => {
                return zerror!(ZErrorKind::Other {
                    descr: format!(
                        "Properties for InfluxDb Storage includes '{}' but not '{}",
                        PROP_BACKEND_USERNAME, PROP_BACKEND_PASSWORD
                    )
                })
            }
            (_, None) => {
                return zerror!(ZErrorKind::Other {
                    descr: format!(
                        "Properties for InfluxDb Storage includes '{}' but not '{}",
                        PROP_BACKEND_PASSWORD, PROP_BACKEND_USERNAME
                    )
                })
            }
        };

        // Check if the database exists (using storages credentials)
        if !is_db_existing(&client, &db).await? {
            if createdb {
                // create db using backend's credentials
                create_db(&self.admin_client, &db, storage_username).await?;
            } else {
                return zerror!(ZErrorKind::Other {
                    descr: format!("Database '{}' doesn't exist in InfluxDb", db)
                });
            }
        }

        // re-insert the actual name of database (in case it has been generated)
        props.insert(PROP_STORAGE_DB.into(), client.database_name().into());
        let admin_status = zenoh::utils::properties_to_json_value(&props);

        // The Influx client on database with backend's credentials (admin), to drop measurements and database
        let mut admin_client = Client::new(self.admin_client.database_url(), db);
        if let Some((username, password)) = &self.credentials {
            admin_client = admin_client.with_auth(username, password);
        }

        Ok(Box::new(InfluxDbStorage {
            admin_status,
            admin_client,
            client,
            path_prefix,
            on_closure,
            timer: Timer::new(),
        }))
    }

    fn incoming_data_interceptor(&self) -> Option<Box<dyn IncomingDataInterceptor>> {
        None
    }

    fn outgoing_data_interceptor(&self) -> Option<Box<dyn OutgoingDataInterceptor>> {
        None
    }
}

enum OnClosure {
    DropDb,
    DropSeries,
    DoNothing,
}

impl TryFrom<&Properties> for OnClosure {
    type Error = ZError;
    fn try_from(p: &Properties) -> ZResult<OnClosure> {
        match p.get(PROP_STORAGE_ON_CLOSURE) {
            Some(s) => {
                if s == "drop_db" {
                    Ok(OnClosure::DropDb)
                } else if s == "drop_series" {
                    Ok(OnClosure::DropSeries)
                } else {
                    zerror!(ZErrorKind::Other {
                        descr: format!("Unsupported value for 'on_closure' property: {}", s)
                    })
                }
            }
            None => Ok(OnClosure::DoNothing),
        }
    }
}

struct InfluxDbStorage {
    admin_status: Value,
    admin_client: Client,
    client: Client,
    path_prefix: Option<String>,
    on_closure: OnClosure,
    timer: Timer,
}

impl InfluxDbStorage {
    async fn get_deletion_timestamp(&self, measurement: &str) -> ZResult<Option<Timestamp>> {
        #[derive(Deserialize, Debug, PartialEq)]
        struct QueryResult {
            timestamp: String,
        }

        let query = <dyn InfluxQuery>::raw_read_query(format!(
            r#"SELECT "timestamp" FROM "{}" WHERE kind='DEL' ORDER BY time DESC LIMIT 1"#,
            measurement
        ));
        match self.client.json_query(query).await {
            Ok(mut result) => match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() && !qr.series[0].values.is_empty() {
                        let ts = qr.series[0].values[0]
                            .timestamp
                            .parse::<Timestamp>()
                            .map_err(|err| {
                                zerror2!(ZErrorKind::Other {
                                    descr: format!(
                                "Failed to parse the latest timestamp for deletion of measurement {} : {}",
                                measurement, err.cause)
                                })
                            })?;
                        Ok(Some(ts))
                    } else {
                        Ok(None)
                    }
                }
                Err(err) => zerror!(ZErrorKind::Other {
                    descr: format!(
                        "Failed to get latest timestamp for deletion of measurement {} : {}",
                        measurement, err
                    )
                }),
            },
            Err(err) => zerror!(ZErrorKind::Other {
                descr: format!(
                    "Failed to get latest timestamp for deletion of measurement {} : {}",
                    measurement, err
                )
            }),
        }
    }

    async fn schedule_measurement_drop(&self, measurement: &str) -> TimedHandle {
        let event = TimedEvent::once(
            Instant::now() + Duration::from_millis(DROP_MEASUREMENT_TIMEOUT_MS),
            TimedMeasurementDrop {
                client: self.admin_client.clone(),
                measurement: measurement.to_string(),
            },
        );
        let handle = event.get_handle();
        self.timer.add(event).await;
        handle
    }
}

#[async_trait]
impl Storage for InfluxDbStorage {
    async fn get_admin_status(&self) -> Value {
        // TODO: possibly add more properties in returned Value for more information about this storage
        self.admin_status.clone()
    }

    // When receiving a Sample (i.e. on PUT or DELETE operations)
    async fn on_sample(&mut self, sample: Sample) -> ZResult<()> {
        let change = Change::from_sample(sample, false)?;

        // measurement is the path, stripped of the path_prefix if any
        let mut measurement = change.path.as_str();
        if let Some(prefix) = &self.path_prefix {
            measurement = measurement.strip_prefix(prefix).ok_or_else(|| {
                zerror2!(ZErrorKind::Other {
                    descr: format!(
                        "Received a Sample not starting with path_prefix '{}'",
                        prefix
                    )
                })
            })?;
        }
        // Note: assume that uhlc timestamp was generated by a clock using UNIX_EPOCH (that's the case by default)
        let influx_time = change.timestamp.get_time().to_duration().as_nanos();

        // Store or delete the sample depending the ChangeKind
        match change.kind {
            ChangeKind::Put => {
                // get timestamp of deletion of this measurement, if any
                if let Some(del_time) = self.get_deletion_timestamp(measurement).await? {
                    // ignore sample if oldest than the deletion
                    if change.timestamp < del_time {
                        debug!("Received a Sample for {} with timestamp older than its deletion; ignore it", change.path);
                        return Ok(());
                    }
                }

                // check that there is a value for this PUT sample
                if change.value.is_none() {
                    return zerror!(ZErrorKind::Other {
                        descr: format!("Received a PUT Sample without value for {}", change.path)
                    });
                }
                // encode the value as a string to be stored in InfluxDB
                let (encoding, base64, value) = change.value.unwrap().encode_to_string();

                // Note: tags are stored as strings in InfluxDB, while fileds are typed.
                // For simpler/faster deserialization, we store encoding, timestamp and base64 as fields.
                // while the kind is stored as a tag to be indexed by InfluxDB and have faster queries on it.
                let query =
                    InfluxWQuery::new(InfluxTimestamp::Nanoseconds(influx_time), measurement)
                        .add_tag("kind", "PUT")
                        .add_field("timestamp", change.timestamp.to_string())
                        .add_field("encoding", encoding)
                        .add_field("base64", base64)
                        .add_field("value", value);
                debug!("Put {} with Influx query: {:?}", change.path, query);
                if let Err(e) = self.client.query(&query).await {
                    return zerror!(ZErrorKind::Other {
                        descr: format!(
                            "Failed to put Value for {} in InfluxDb storage : {}",
                            change.path, e
                        )
                    });
                }
            }
            ChangeKind::Delete => {
                // delete all points from the measurement that are older than this DELETE message
                // (in case more recent PUT have been recevived un-ordered)
                let query = <dyn InfluxQuery>::raw_read_query(format!(
                    r#"DELETE FROM "{}" WHERE time < {}"#,
                    measurement, influx_time
                ));
                debug!("Delete {} with Influx query: {:?}", change.path, query);
                if let Err(e) = self.client.query(&query).await {
                    return zerror!(ZErrorKind::Other {
                        descr: format!(
                            "Failed to delete points for measurement '{}' from InfluxDb storage : {}",
                            measurement, e
                        )
                    });
                }
                // store a point (with timestamp) with "delete" tag, thus we don't re-introduce an older point later
                let query =
                    InfluxWQuery::new(InfluxTimestamp::Nanoseconds(influx_time), measurement)
                        .add_field("timestamp", change.timestamp.to_string())
                        .add_tag("kind", "DEL");
                debug!(
                    "Mark measurement {} as deleted at time {}",
                    measurement, influx_time
                );
                if let Err(e) = self.client.query(&query).await {
                    return zerror!(ZErrorKind::Other {
                        descr: format!(
                            "Failed to mark measurement {} as deleted : {}",
                            change.path, e
                        )
                    });
                }
                // schedule the drop of measurement later in the future, if it's empty
                let _ = self.schedule_measurement_drop(measurement).await;
            }
            ChangeKind::Patch => {
                println!("Received PATCH for {}: not yet supported", change.path);
            }
        }
        Ok(())
    }

    // When receiving a Query (i.e. on GET operations)
    async fn on_query(&mut self, query: Query) -> ZResult<()> {
        // get the query's Selector
        let selector = Selector::try_from(&query)?;

        // if a path_prefix is used
        let regex = if let Some(prefix) = &self.path_prefix {
            // get the list of sub-path expressions that will match the same stored keys than
            // the selector, if those keys had the path_prefix.
            let path_exprs = utils::get_sub_path_exprs(selector.path_expr.as_str(), prefix);
            debug!(
                "Query on {} with path_expr={} => sub_path_exprs = {:?}",
                selector.path_expr, prefix, path_exprs
            );
            // convert the sub-path expressions into an Influx regex
            path_exprs_to_influx_regex(&path_exprs)
        } else {
            // convert the Selector's path expression into an Influx regex
            path_exprs_to_influx_regex(&[selector.path_expr.as_str()])
        };

        // construct the Influx query clauses from the Selector
        let clauses = clauses_from_selector(&selector);

        // the Influx query
        let influx_query_str = format!("SELECT * FROM {} {}", regex, clauses);
        let influx_query = <dyn InfluxQuery>::raw_read_query(&influx_query_str);

        // the expected JSon type resulting from the query
        #[derive(Deserialize, Debug)]
        struct ZenohPoint {
            kind: String,
            timestamp: String,
            encoding: zenoh::net::ZInt,
            base64: bool,
            value: String,
        }
        debug!("Get {} with Influx query: {}", selector, influx_query_str);
        match self.client.json_query(influx_query).await {
            Ok(mut query_result) => {
                while !query_result.results.is_empty() {
                    match query_result.deserialize_next::<ZenohPoint>() {
                        Ok(retn) => {
                            for serie in retn.series {
                                // reconstruct the path from the measurement name (same as serie.name)
                                let mut res_name = String::with_capacity(serie.name.len());
                                if let Some(p) = &self.path_prefix {
                                    res_name.push_str(p);
                                }
                                res_name.push_str(&serie.name);
                                debug!("Replying {} values for {}", serie.values.len(), res_name);
                                for zpoint in serie.values {
                                    // decode the value and the timestamp
                                    match (
                                        Value::decode_from_string(
                                            zpoint.encoding,
                                            zpoint.base64,
                                            zpoint.value,
                                        ),
                                        Timestamp::from_str(&zpoint.timestamp),
                                    ) {
                                        (Ok(value), Ok(timestamp)) => {
                                            let (encoding, payload) = value.encode();
                                            let mut info = DataInfo::new();
                                            info.encoding = Some(encoding);
                                            info.timestamp = Some(timestamp);
                                            let data_info = Some(info);
                                            query
                                                .reply(Sample {
                                                    res_name: res_name.clone(),
                                                    payload,
                                                    data_info,
                                                })
                                                .await;
                                        }
                                        (Err(e), _) => warn!(
                                            r#"Failed to decode zenoh Value from Influx point {} with timestamp="{}": {}"#,
                                            serie.name, zpoint.timestamp, e
                                        ),
                                        (_, Err(e)) => warn!(
                                            r#"Failed to decode zenoh Timestamp from Influx point {} with timestamp="{}": {:?}"#,
                                            serie.name, zpoint.timestamp, e
                                        ),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            return zerror!(ZErrorKind::Other {
                                descr: format!(
                                    "Failed to parse result of InfluxDB query '{}': {}",
                                    influx_query_str, e
                                )
                            })
                        }
                    }
                }
                Ok(())
            }
            Err(e) => zerror!(ZErrorKind::Other {
                descr: format!(
                    "Failed to query InfluxDb with '{}' : {}",
                    influx_query_str, e
                )
            }),
        }
    }
}

impl Drop for InfluxDbStorage {
    fn drop(&mut self) {
        debug!("Closing InfluxDB storage");
        match self.on_closure {
            OnClosure::DropDb => {
                let _ = task::block_on(async move {
                    let db = self.admin_client.database_name();
                    debug!("Close InfluxDB storage, dropping database {}", db);
                    let query = <dyn InfluxQuery>::raw_read_query(format!("DROP DATABASE {}", db));
                    if let Err(e) = self.admin_client.query(&query).await {
                        error!("Failed to drop InfluxDb database '{}' : {}", db, e)
                    }
                });
            }
            OnClosure::DropSeries => {
                let _ = task::block_on(async move {
                    let db = self.client.database_name();
                    debug!(
                        "Close InfluxDB storage, dropping all series from database {}",
                        db
                    );
                    let query = <dyn InfluxQuery>::raw_read_query("DROP SERIES FROM /.*/");
                    if let Err(e) = self.client.query(&query).await {
                        error!(
                            "Failed to drop all series from InfluxDb database '{}' : {}",
                            db, e
                        )
                    }
                });
            }
            OnClosure::DoNothing => {
                debug!(
                    "Close InfluxDB storage, keeping database {} as it is",
                    self.client.database_name()
                );
            }
        }
    }
}

// Scheduled dropping of a measurement after a timeout, if it's empty
struct TimedMeasurementDrop {
    client: Client,
    measurement: String,
}

#[async_trait]
impl Timed for TimedMeasurementDrop {
    async fn run(&mut self) {
        #[derive(Deserialize, Debug, PartialEq)]
        struct QueryResult {
            kind: String,
        }

        // check if there is at least 1 point without "DEL" kind in the measurement
        let query = <dyn InfluxQuery>::raw_read_query(format!(
            r#"SELECT "kind" FROM "{}" WHERE kind!='DEL' LIMIT 1"#,
            self.measurement
        ));
        match self.client.json_query(query).await {
            Ok(mut result) => match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() {
                        debug!("Measurement {} contains new values inserted after deletion; don't drop it", self.measurement);
                        return;
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                        self.measurement, e
                    );
                }
            },
            Err(e) => {
                warn!(
                    "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                    self.measurement, e
                );
                return;
            }
        }

        // drop the measurement
        let query = <dyn InfluxQuery>::raw_read_query(format!(
            r#"DROP MEASUREMENT "{}""#,
            self.measurement
        ));
        debug!(
            "Drop measurement {} after timeout with Influx query: {:?}",
            self.measurement, query
        );
        if let Err(e) = self.client.query(&query).await {
            warn!(
                "Failed to drop measurement '{}' from InfluxDb storage : {}",
                self.measurement, e
            );
        }
    }
}

fn generate_db_name() -> String {
    format!("zenoh_db_{}", Uuid::new_v4().to_simple())
}

async fn is_db_existing(client: &Client, db_name: &str) -> ZResult<bool> {
    #[derive(Deserialize)]
    struct Database {
        name: String,
    }
    let query = <dyn InfluxQuery>::raw_read_query("SHOW DATABASES");
    debug!("List databases with Influx query: {:?}", query);
    match client.json_query(query).await {
        Ok(mut result) => match result.deserialize_next::<Database>() {
            Ok(dbs) => {
                for serie in dbs.series {
                    for db in serie.values {
                        if db_name == db.name {
                            return Ok(true);
                        }
                    }
                }
                // not found
                Ok(false)
            }
            Err(e) => zerror!(ZErrorKind::Other {
                descr: format!(
                    "Failed to parse list of existing InfluxDb databases : {}",
                    e
                )
            }),
        },
        Err(e) => zerror!(ZErrorKind::Other {
            descr: format!("Failed to list existing InfluxDb databases : {}", e)
        }),
    }
}

async fn create_db(
    client: &Client,
    db_name: &str,
    storage_username: Option<String>,
) -> ZResult<()> {
    let query = <dyn InfluxQuery>::raw_read_query(format!("CREATE DATABASE {}", db_name));
    debug!("Create Influx database: {}", db_name);
    if let Err(e) = client.query(&query).await {
        return zerror!(ZErrorKind::Other {
            descr: format!(
                "Failed to create new InfluxDb database '{}' : {}",
                db_name, e
            )
        });
    }

    // is a username is specified for storage access, grant him access to the database
    if let Some(username) = storage_username {
        let query =
            <dyn InfluxQuery>::raw_read_query(format!("GRANT ALL ON {} TO {}", db_name, username));
        debug!(
            "Grant access to {} on Influx database: {}",
            username, db_name
        );
        if let Err(e) = client.query(&query).await {
            return zerror!(ZErrorKind::Other {
                descr: format!(
                    "Failed grant access to {} on Influx database '{}' : {}",
                    username, db_name, e
                )
            });
        }
    }
    Ok(())
}

// Returns an InfluxDB regex (see https://docs.influxdata.com/influxdb/v1.8/query_language/explore-data/#regular-expressions)
// corresponding to the list of path expressions. I.e.:
// Replace "**" with ".*", "*" with "[^\/]*"  and "/" with "\/".
// Concat each with "|", and surround the result with '/^' and '$/'.
fn path_exprs_to_influx_regex(path_exprs: &[&str]) -> String {
    let mut result = String::with_capacity(2 * path_exprs[0].len());
    result.push_str("/^");
    for (i, path_expr) in path_exprs.iter().enumerate() {
        if i != 0 {
            result.push('|');
        }
        let mut chars = path_expr.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if let Some(c2) = chars.peek() {
                        if c2 == &'*' {
                            result.push_str(".*");
                            chars.next();
                        } else {
                            result.push_str(".*")
                        }
                    }
                }
                '/' => result.push_str(r#"\/"#),
                _ => result.push(c),
            }
        }
    }
    result.push_str("$/");
    result
}

fn clauses_from_selector(s: &Selector) -> String {
    let mut result = String::with_capacity(256);
    result.push_str("WHERE kind!='DEL'");
    match (s.properties.get("starttime"), s.properties.get("stoptime")) {
        (Some(start), Some(stop)) => {
            result.push_str(" AND time >= ");
            result.push_str(&normalize_rfc3339(start));
            result.push_str(" AND time <= ");
            result.push_str(&normalize_rfc3339(stop));
        }
        (Some(start), None) => {
            result.push_str(" AND time >= ");
            result.push_str(&normalize_rfc3339(start));
        }
        (None, Some(stop)) => {
            result.push_str(" AND time <= ");
            result.push_str(&normalize_rfc3339(stop));
        }
        _ => {
            //No time selection, return only latest values
            result.push_str(" ORDER BY time DESC LIMIT 1");
        }
    }
    result
}

// Surrounds with `''` all parts of `time` matching a RFC3339 time representation
// to comply with InfluxDB time clauses.
fn normalize_rfc3339(time: &str) -> Cow<str> {
    lazy_static::lazy_static! {
        static ref RE: Regex = Regex::new(
            "(?:'?(?P<rfc3339>[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9][ T]?[0-9:.]*Z?)'?)"
        )
        .unwrap();
    }

    RE.replace_all(time, "'$rfc3339'")
}

#[test]
fn test_normalize_rfc3339() {
    // test no surrounding with '' if not rfc3339 time
    assert_eq!("now()", normalize_rfc3339("now()"));
    assert_eq!("now()-1h", normalize_rfc3339("now()-1h"));

    // test surrounding with ''
    assert_eq!(
        "'2020-11-05T16:31:42.226942997Z'",
        normalize_rfc3339("2020-11-05T16:31:42.226942997Z")
    );
    assert_eq!(
        "'2020-11-05T16:31:42Z'",
        normalize_rfc3339("2020-11-05T16:31:42Z")
    );
    assert_eq!(
        "'2020-11-05 16:31:42.226942997'",
        normalize_rfc3339("2020-11-05 16:31:42.226942997")
    );
    assert_eq!("'2020-11-05'", normalize_rfc3339("2020-11-05"));

    // test no surrounding with '' if already done
    assert_eq!(
        "'2020-11-05T16:31:42.226942997Z'",
        normalize_rfc3339("'2020-11-05T16:31:42.226942997Z'")
    );

    // test surrounding with '' only the rfc3339 time
    assert_eq!(
        "'2020-11-05T16:31:42.226942997Z'-1h",
        normalize_rfc3339("2020-11-05T16:31:42.226942997Z-1h")
    );
    assert_eq!(
        "'2020-11-05T16:31:42Z'-1h",
        normalize_rfc3339("2020-11-05T16:31:42Z-1h")
    );
    assert_eq!(
        "'2020-11-05 16:31:42.226942997'-1h",
        normalize_rfc3339("2020-11-05 16:31:42.226942997-1h")
    );
    assert_eq!("'2020-11-05'-1h", normalize_rfc3339("2020-11-05-1h"));
}
