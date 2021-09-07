mod config;
mod database;
mod datetime;
mod model;
mod numerism;
mod templating;


use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;
use std::result::Result;

use chrono::{Duration, Local, Timelike};
use env_logger;
use form_urlencoded;
use http::request::Parts;
use hyper::{Body, Method, Request, Response, Server};
use hyper::body;
use hyper::service::{make_service_fn, service_fn};
use log::error;
use num_rational::Rational32;
use num_traits::Zero;
use once_cell::sync::{Lazy, OnceCell};
use regex::Regex;
use tera::{Context, Tera};
use tokio::sync::RwLock;
use toml;
use url::Url;

use crate::config::{AuthToken, CONFIG, CONFIG_PATH, load_config};
use crate::database::{
    add_blood_pressure_measurement, add_blood_sugar_measurement, add_mass_measurement,
    add_temperature_measurement, get_recent_blood_pressure_measurements,
    get_recent_blood_sugar_measurements, get_recent_mass_measurements,
    get_recent_temperature_measurements, get_temperature_locations,
};
use crate::model::{
    DailyBloodPressureMeasurements, BloodPressureMeasurement, BloodSugarMeasurement,
    BodyMassMeasurement, BodyTemperatureMeasurement,
};
use crate::numerism::{ParseRationalError, r32_from_decimal};
use crate::templating::RatioToFloat;


const ABSOLUTE_ZERO_CELSIUS: Lazy<Rational32> = Lazy::new(|| Rational32::new(-27315, 100));
static TERA: OnceCell<RwLock<Tera>> = OnceCell::new();
static STATIC_PATH_RE: Lazy<Regex> = Lazy::new(|| Regex::new("^/static/([a-z0-9-._]+)$").unwrap());


#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MissingValueError(String);
impl fmt::Display for MissingValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "missing value for key {}", self.0)
    }
}
impl Error for MissingValueError {
}


#[derive(Debug)]
pub(crate) enum ServerError {
    OpeningConfigFile(std::io::Error),
    ReadingConfigFile(std::io::Error),
    ParsingConfigFile(toml::de::Error),
    ParsingListenAddress(AddrParseError),
    HyperError(hyper::Error),
    TemplatingSetup(tera::Error),
}
impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::OpeningConfigFile(e)
                => write!(f, "error opening config file: {}", e),
            ServerError::ReadingConfigFile(e)
                => write!(f, "error reading config file: {}", e),
            ServerError::ParsingConfigFile(e)
                => write!(f, "error parsing config file: {}", e),
            ServerError::ParsingListenAddress(e)
                => write!(f, "error parsing listen address: {}", e),
            ServerError::HyperError(e)
                => write!(f, "hyper error: {}", e),
            ServerError::TemplatingSetup(e)
                => write!(f, "error setting up templating: {}", e),
        }
    }
}
impl Error for ServerError {
}


#[derive(Debug)]
pub(crate) enum ClientError {
    MissingValue(String),
    FailedToParseIntValue(String, String, std::num::ParseIntError),
    FailedToParseRationalValue(String, String, ParseRationalError),
    IntValueZeroOrLess(String, i32),
    RationalValueZeroOrLess(String, Rational32),
    IntValueTooHigh(String, i32, i32),
    RationalValueTooLow(String, Rational32, Rational32),
    ValueIsInvalidOption(String, String, Vec<String>),
}
impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::MissingValue(key)
                => write!(f, "missing value for key: {}", key),
            ClientError::FailedToParseIntValue(key, value, err)
                => write!(f, "failed to parse value {:?} for key {:?} as integer: {}", value, key, err),
            ClientError::FailedToParseRationalValue(key, value, err)
                => write!(f, "failed to parse value {:?} for key {:?} as a rational number: {}", value, key, err),
            ClientError::IntValueZeroOrLess(key, value)
                => write!(f, "value {} for key {:?} is zero or less", value, key),
            ClientError::RationalValueZeroOrLess(key, value)
                => write!(f, "value {} for key {:?} is zero or less", value, key),
            ClientError::IntValueTooHigh(key, value, max)
                => write!(f, "value {} for key {:?} is too high (> {})", value, key, max),
            ClientError::RationalValueTooLow(key, value, min)
                => write!(f, "value {} for key {:?} is too low (< {})", value, key, min),
            ClientError::ValueIsInvalidOption(key, value, valid_options)
                => write!(f, "value {} for key {:?} is not a valid option; valid options are {:?}", value, key, valid_options),
        }
    }
}
impl Error for ClientError {
}


async fn render_template(template_name: &str, context: &Context) -> Result<Body, tera::Error> {
    let template_string = {
        TERA.get()
            .expect("template engine is set")
            .read()
            .await
            .render(template_name, context)?
    };
    let body = Body::from(template_string);
    Ok(body)
}

async fn respond_template(
    template_name: &str,
    context: &Context,
    status: u16,
    headers: &HashMap<String, String>,
) -> Result<Response<Body>, Infallible> {
    let body = match render_template(template_name, context).await {
        Ok(b) => b,
        Err(e) => {
            error!("failed to render template: {:?}", e);
            return respond_500();
        },
    };

    let mut response_builder = Response::builder()
        .status(status)
        .header("Content-Type", "text/html; charset=utf-8");

    for (key, value) in headers {
        response_builder = response_builder.header(key, value);
    }

    let response = match response_builder.body(body) {
        Ok(r) => r,
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        }
    };
    Ok(response)
}

fn respond_500() -> Result<Response<Body>, Infallible> {
    let body = Body::from(String::from(
        r#"<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" lang="en" xml:lang="en">
<head>
<meta charset="utf-8" />
<title>Internal Server Error</title>
</head>
<body>
<h1>Internal Server Error</h1>
<p>Something went wrong. It's not your fault. Tell the people responsible to check the logs.</p>
</body>
</html>"#
    ));

    // can't do much except unwrap/expect here, as this *is* the error handler
    let response = Response::builder()
        .header("Content-Type", "text/html; charset=utf-8")
        .body(body)
        .expect("failed to create response");
    Ok(response)
}

async fn respond_400(err: ClientError) -> Result<Response<Body>, Infallible> {
    let mut context = Context::new();
    context.insert("error", &err.to_string());

    respond_template(
        "400.html.tera",
        &context,
        400,
        &HashMap::new(),
    ).await
}

async fn respond_403() -> Result<Response<Body>, Infallible> {
    respond_template(
        "403.html.tera",
        &Context::new(),
        403,
        &HashMap::new(),
    ).await
}

async fn respond_403_ro() -> Result<Response<Body>, Infallible> {
    let mut headers = HashMap::new();
    headers.insert(
        "Forbidden-Reason".to_owned(),
        "token-read-only".to_owned(),
    );
    respond_template(
        "403_ro.html.tera",
        &Context::new(),
        403,
        &headers,
    ).await
}

async fn respond_404() -> Result<Response<Body>, Infallible> {
    respond_template(
        "404.html.tera",
        &Context::new(),
        404,
        &HashMap::new(),
    ).await
}

async fn respond_405(allowed_methods: &[Method]) -> Result<Response<Body>, Infallible> {
    let methods: Vec<String> = allowed_methods.iter()
        .map(|m| m.to_string())
        .collect();
    let joined_methods = methods.join(", ");

    let mut context = Context::new();
    context.insert("allowed_methods", &methods);

    let mut headers = HashMap::new();
    headers.insert(String::from("Allow"), joined_methods);

    respond_template(
        "405.html.tera",
        &context,
        405,
        &headers,
    ).await
}

async fn redirect_to_self(parts: Parts) -> Result<Response<Body>, Infallible> {
    let req_uri_string = parts.uri.to_string();
    let req_uri_noslash = req_uri_string.trim_start_matches('/');

    let base_uri: Url = {
        let base_uri_str = &CONFIG
            .get().expect("cannot get config")
            .read().await
            .base_url;
        match base_uri_str.parse() {
            Ok(bus) => bus,
            Err(e) => {
                error!("failed to parse URI {:?}: {}", base_uri_str, e);
                return respond_500();
            },
        }
    };
    let page_uri = match base_uri.join(&req_uri_noslash) {
        Ok(pu) => pu,
        Err(e) => {
            error!("failed to join {} and {}: {}", base_uri, req_uri_noslash, e);
            return respond_500();
        }
    };
    let page_uri_string = page_uri.to_string();

    let mut context = Context::new();
    context.insert("url", &page_uri_string);

    let mut headers = HashMap::new();
    headers.insert(String::from("Location"), page_uri_string);

    respond_template(
        "redirect.html.tera",
        &context,
        302,
        &headers,
    ).await
}

async fn get_index(token: &AuthToken) -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_blood_pressure_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };
    recent_measurements.sort_by_key(|m| m.timestamp);

    let measurements_with_spo2: Vec<BloodPressureMeasurement> = recent_measurements.iter()
        .filter(|m| m.spo2_percent.is_some())
        .map(|m| m.clone())
        .collect();

    // group measurements by day
    let hours = {
        let config_guard = CONFIG
            .get().unwrap()
            .read().await;
        config_guard.hours
    };
    let mut day_to_measurements: BTreeMap<String, DailyBloodPressureMeasurements> = BTreeMap::new();
    let mut max_measurement: Option<BloodPressureMeasurement> = None;
    let mut min_measurement: Option<BloodPressureMeasurement> = None;
    for measurement in &recent_measurements {
        if let Some(mm) = &mut max_measurement {
            *mm = mm.values_max(&measurement);
        } else {
            max_measurement = Some(measurement.clone());
        }

        if let Some(mm) = &mut min_measurement {
            *mm = mm.values_min(&measurement);
        } else {
            min_measurement = Some(measurement.clone());
        }

        let mut day = measurement.timestamp.date().naive_local();
        if measurement.timestamp.hour() < hours.morning_start {
            // count this as (the evening of) the previous day
            day = day.pred();
        }

        let date_string = day.format("%Y-%m-%d").to_string();

        let entry = day_to_measurements
            .entry(date_string.clone())
            .or_insert_with(|| DailyBloodPressureMeasurements::new_empty(date_string));

        let this_hour = measurement.timestamp.hour();

        if this_hour < hours.morning_start && entry.evening.is_none() {
            // night (previous day)
            entry.evening = Some(measurement.clone());
        } else if this_hour >= hours.morning_start && this_hour < hours.morning_end && entry.morning.is_none() {
            // morning
            entry.morning = Some(measurement.clone());
        } else if this_hour >= hours.midday_start && this_hour < hours.midday_end && entry.midday.is_none() {
            // midday
            entry.midday = Some(measurement.clone());
        } else if this_hour >= hours.evening_start && entry.evening.is_none() {
            // night
            entry.evening = Some(measurement.clone());
        } else {
            entry.other.push(measurement.clone());
        }
    }

    let days_and_measurements: Vec<DailyBloodPressureMeasurements> = day_to_measurements
        .values()
        .rev()
        .map(|v| v.clone())
        .collect();

    let mut context = Context::new();
    context.insert("token", &token);
    context.insert("measurements", &recent_measurements);
    context.insert("measurements_with_spo2", &measurements_with_spo2);
    context.insert("days_and_measurements", &days_and_measurements);

    if days_and_measurements.len() > 0 {
        // calculate percentiles
        let average = BloodPressureMeasurement::average(&recent_measurements);
        let quasi_q1 = BloodPressureMeasurement::quasi_n_tile(&recent_measurements, 1, 4);
        let quasi_q2 = BloodPressureMeasurement::quasi_n_tile(&recent_measurements, 1, 2);
        let quasi_q3 = BloodPressureMeasurement::quasi_n_tile(&recent_measurements, 3, 4);

        context.insert("max_measurement", &max_measurement);
        context.insert("quasi_q3_measurement", &quasi_q3);
        context.insert("avg_measurement", &average);
        context.insert("quasi_q2_measurement", &quasi_q2);
        context.insert("quasi_q1_measurement", &quasi_q1);
        context.insert("min_measurement", &min_measurement);
    }

    respond_template(
        "list.html.tera",
        &context,
        200,
        &HashMap::new(),
    ).await
}

async fn get_mass(token: &AuthToken) -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_mass_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };
    recent_measurements.sort_by_key(|m| m.timestamp);
    recent_measurements.reverse();

    let mut max_measurement: Option<BodyMassMeasurement> = None;
    let mut min_measurement: Option<BodyMassMeasurement> = None;
    for measurement in &recent_measurements {
        if let Some(mm) = &mut max_measurement {
            *mm = mm.values_max(&measurement);
        } else {
            max_measurement = Some(measurement.clone());
        }

        if let Some(mm) = &mut min_measurement {
            *mm = mm.values_min(&measurement);
        } else {
            min_measurement = Some(measurement.clone());
        }
    }

    let mut context = Context::new();
    context.insert("token", &token);
    context.insert("measurements", &recent_measurements);

    if recent_measurements.len() > 0 {
        // calculate percentiles
        let average = BodyMassMeasurement::average(&recent_measurements);
        let quasi_q1 = BodyMassMeasurement::quasi_n_tile(&recent_measurements, 1, 4);
        let quasi_q2 = BodyMassMeasurement::quasi_n_tile(&recent_measurements, 1, 2);
        let quasi_q3 = BodyMassMeasurement::quasi_n_tile(&recent_measurements, 3, 4);

        context.insert("max_measurement", &max_measurement);
        context.insert("quasi_q3_measurement", &quasi_q3);
        context.insert("avg_measurement", &average);
        context.insert("quasi_q2_measurement", &quasi_q2);
        context.insert("quasi_q1_measurement", &quasi_q1);
        context.insert("min_measurement", &min_measurement);
    }

    respond_template(
        "mass_list.html.tera",
        &context,
        200,
        &HashMap::new(),
    ).await
}

async fn get_temperature(token: &AuthToken) -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_temperature_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };
    recent_measurements.sort_by_key(|m| m.timestamp);
    recent_measurements.reverse();

    let mut max_measurement: Option<BodyTemperatureMeasurement> = None;
    let mut min_measurement: Option<BodyTemperatureMeasurement> = None;
    for measurement in &recent_measurements {
        if let Some(mm) = &mut max_measurement {
            *mm = mm.values_max(&measurement);
        } else {
            max_measurement = Some(measurement.clone());
        }

        if let Some(mm) = &mut min_measurement {
            *mm = mm.values_min(&measurement);
        } else {
            min_measurement = Some(measurement.clone());
        }
    }

    let locations = match get_temperature_locations().await {
        Ok(l) => l,
        Err(e) => {
            error!("error obtaining temperature locations: {}", e);
            return respond_500();
        }
    };
    let location_id_to_name: HashMap<i64, String> = locations
        .iter()
        .map(|loc| (loc.id, loc.name.clone()))
        .collect();

    let mut context = Context::new();
    context.insert("token", &token);
    context.insert("measurements", &recent_measurements);
    context.insert("temperature_locations", &locations);
    context.insert("temperature_location_id_to_name", &location_id_to_name);
    {
        let config = CONFIG
            .get().unwrap()
            .read().await;
        context.insert("default_temperature_location_id", &config.default_temperature_location_id);
    }

    if recent_measurements.len() > 0 {
        // calculate percentiles
        let average = BodyTemperatureMeasurement::average(&recent_measurements);
        let quasi_q1 = BodyTemperatureMeasurement::quasi_n_tile(&recent_measurements, 1, 4);
        let quasi_q2 = BodyTemperatureMeasurement::quasi_n_tile(&recent_measurements, 1, 2);
        let quasi_q3 = BodyTemperatureMeasurement::quasi_n_tile(&recent_measurements, 3, 4);

        context.insert("max_measurement", &max_measurement);
        context.insert("quasi_q3_measurement", &quasi_q3);
        context.insert("avg_measurement", &average);
        context.insert("quasi_q2_measurement", &quasi_q2);
        context.insert("quasi_q1_measurement", &quasi_q1);
        context.insert("min_measurement", &min_measurement);
    }

    respond_template(
        "temperature_list.html.tera",
        &context,
        200,
        &HashMap::new(),
    ).await
}

async fn get_sugar(token: &AuthToken) -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_blood_sugar_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };
    recent_measurements.sort_by_key(|m| m.timestamp);
    recent_measurements.reverse();

    let mut max_measurement: Option<BloodSugarMeasurement> = None;
    let mut min_measurement: Option<BloodSugarMeasurement> = None;
    for measurement in &recent_measurements {
        if let Some(mm) = &mut max_measurement {
            *mm = mm.values_max(&measurement);
        } else {
            max_measurement = Some(measurement.clone());
        }

        if let Some(mm) = &mut min_measurement {
            *mm = mm.values_min(&measurement);
        } else {
            min_measurement = Some(measurement.clone());
        }
    }

    let mut context = Context::new();
    context.insert("token", &token);
    context.insert("measurements", &recent_measurements);

    if recent_measurements.len() > 0 {
        // calculate percentiles
        let average = BloodSugarMeasurement::average(&recent_measurements);
        let quasi_q1 = BloodSugarMeasurement::quasi_n_tile(&recent_measurements, 1, 4);
        let quasi_q2 = BloodSugarMeasurement::quasi_n_tile(&recent_measurements, 1, 2);
        let quasi_q3 = BloodSugarMeasurement::quasi_n_tile(&recent_measurements, 3, 4);

        context.insert("max_measurement", &max_measurement);
        context.insert("quasi_q3_measurement", &quasi_q3);
        context.insert("avg_measurement", &average);
        context.insert("quasi_q2_measurement", &quasi_q2);
        context.insert("quasi_q1_measurement", &quasi_q1);
        context.insert("min_measurement", &min_measurement);
    }

    respond_template(
        "sugar_list.html.tera",
        &context,
        200,
        &HashMap::new(),
    ).await
}

async fn get_api_bp() -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_blood_pressure_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };

    recent_measurements.sort_by_key(|m| m.timestamp);

    // make it a JSON
    let recent_json = match serde_json::to_string(&recent_measurements) {
        Ok(rj) => rj,
        Err(e) => {
            error!("error serializing recent measurements to JSON: {}", e);
            return respond_500();
        },
    };

    // spit it out
    let response_res = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(Body::from(recent_json));
    match response_res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        },
    }
}

async fn get_api_mass() -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_mass_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };

    recent_measurements.sort_by_key(|m| m.timestamp);

    // make it a JSON
    let recent_json = match serde_json::to_string(&recent_measurements) {
        Ok(rj) => rj,
        Err(e) => {
            error!("error serializing recent measurements to JSON: {}", e);
            return respond_500();
        },
    };

    // spit it out
    let response_res = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(Body::from(recent_json));
    match response_res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        },
    }
}

async fn get_api_temperature() -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_temperature_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };

    recent_measurements.sort_by_key(|m| m.timestamp);

    // make it a JSON
    let recent_json = match serde_json::to_string(&recent_measurements) {
        Ok(rj) => rj,
        Err(e) => {
            error!("error serializing recent measurements to JSON: {}", e);
            return respond_500();
        },
    };

    // spit it out
    let response_res = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(Body::from(recent_json));
    match response_res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        },
    }
}

async fn get_api_sugar() -> Result<Response<Body>, Infallible> {
    let mut recent_measurements = match get_recent_blood_sugar_measurements(Duration::days(3*31)).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error obtaining recent measurements: {}", e);
            return respond_500();
        },
    };

    recent_measurements.sort_by_key(|m| m.timestamp);

    // make it a JSON
    let recent_json = match serde_json::to_string(&recent_measurements) {
        Ok(rj) => rj,
        Err(e) => {
            error!("error serializing recent measurements to JSON: {}", e);
            return respond_500();
        },
    };

    // spit it out
    let response_res = Response::builder()
        .status(200)
        .header("Content-Type", "application/json")
        .body(Body::from(recent_json));
    match response_res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        },
    }
}

fn get_form_i32_gt0(req_kv: &HashMap<String, String>, key: &str) -> Result<Option<i32>, ClientError> {
    let string_value = match req_kv.get(key) {
        Some(sv) => sv,
        None => return Ok(None),
    };
    let i32_value: i32 = string_value.parse()
        .map_err(|e| ClientError::FailedToParseIntValue(String::from(key), string_value.clone(), e))?;
    if i32_value < 0 {
        Err(ClientError::IntValueZeroOrLess(String::from(key), i32_value))
    } else {
        Ok(Some(i32_value))
    }
}

fn get_req_form_i32_gt0(req_kv: &HashMap<String, String>, key: &str) -> Result<i32, ClientError> {
    match get_form_i32_gt0(req_kv, key) {
        Ok(Some(i)) => Ok(i),
        Ok(None) => Err(ClientError::MissingValue(String::from(key))),
        Err(e) => Err(e),
    }
}

fn get_form_i64(req_kv: &HashMap<String, String>, key: &str) -> Result<Option<i64>, ClientError> {
    let string_value = match req_kv.get(key) {
        Some(sv) => sv,
        None => return Ok(None),
    };
    let i64_value: i64 = string_value.parse()
        .map_err(|e| ClientError::FailedToParseIntValue(String::from(key), string_value.clone(), e))?;
    Ok(Some(i64_value))
}

fn get_req_form_i64(req_kv: &HashMap<String, String>, key: &str) -> Result<i64, ClientError> {
    match get_form_i64(req_kv, key) {
        Ok(Some(i)) => Ok(i),
        Ok(None) => Err(ClientError::MissingValue(String::from(key))),
        Err(e) => Err(e),
    }
}

fn get_form_r32(req_kv: &HashMap<String, String>, key: &str) -> Result<Option<Rational32>, ClientError> {
    let string_value = match req_kv.get(key) {
        Some(sv) => sv,
        None => return Ok(None),
    };
    let r32_value: Rational32 = r32_from_decimal(string_value)
        .map_err(|e| ClientError::FailedToParseRationalValue(String::from(key), string_value.clone(), e))?;
    Ok(Some(r32_value))
}

fn get_form_r32_gt0(req_kv: &HashMap<String, String>, key: &str) -> Result<Option<Rational32>, ClientError> {
    match get_form_r32(req_kv, key)? {
        Some(v) => {
            if v < Zero::zero() {
                Err(ClientError::RationalValueZeroOrLess(String::from(key), v))
            } else {
                Ok(Some(v))
            }
        },
        None => Ok(None),
    }
}

fn get_req_form_r32(req_kv: &HashMap<String, String>, key: &str) -> Result<Rational32, ClientError> {
    match get_form_r32(req_kv, key) {
        Ok(Some(i)) => Ok(i),
        Ok(None) => Err(ClientError::MissingValue(String::from(key))),
        Err(e) => Err(e),
    }
}

fn get_req_form_r32_gt0(req_kv: &HashMap<String, String>, key: &str) -> Result<Rational32, ClientError> {
    match get_form_r32_gt0(req_kv, key) {
        Ok(Some(i)) => Ok(i),
        Ok(None) => Err(ClientError::MissingValue(String::from(key))),
        Err(e) => Err(e),
    }
}

fn get_measurement_from_form(req_kv: &HashMap<String, String>) -> Result<BloodPressureMeasurement, ClientError> {
    let systolic_mmhg: i32 = get_req_form_i32_gt0(&req_kv, "systolic_mmhg")?;
    let diastolic_mmhg: i32 = get_req_form_i32_gt0(&req_kv, "diastolic_mmhg")?;
    let pulse_bpm: i32 = get_req_form_i32_gt0(&req_kv, "pulse_bpm")?;
    let spo2_percent: Option<i32> = get_form_i32_gt0(&req_kv, "spo2_percent")?;

    if let Some(sat) = spo2_percent {
        if sat > 100 {
            return Err(ClientError::IntValueTooHigh("spo2_percent".into(), sat, 100));
        }
    }

    let local_now = Local::now();
    let measurement = BloodPressureMeasurement::new(
        -1,
        local_now,
        systolic_mmhg,
        diastolic_mmhg,
        pulse_bpm,
        spo2_percent,
    );
    Ok(measurement)
}

async fn get_mass_measurement_from_form(req_kv: &HashMap<String, String>) -> Result<BodyMassMeasurement, ClientError> {
    let mass_kg: Rational32 = get_req_form_r32_gt0(&req_kv, "mass_kg")?;

    let height_cm: Option<i32> = {
        let config_guard = CONFIG
            .get().expect("initial config not set")
            .read().await;
        config_guard.height_cm
    };
    let height_m = height_cm
        .map(|h| Rational32::new(h, 100));
    let square_height_m2 = height_m
        .map(|h| h * h);
    let bmi: Option<Rational32> = square_height_m2.map(|sqh|
        mass_kg / sqh
    );

    let local_now = Local::now();
    let measurement = BodyMassMeasurement::new(
        -1,
        local_now,
        mass_kg,
        bmi,
    );
    Ok(measurement)
}

async fn get_temperature_measurement_from_form(req_kv: &HashMap<String, String>) -> Result<BodyTemperatureMeasurement, ClientError> {
    let location_id: i64 = get_req_form_i64(req_kv, "location")?;

    let temp_celsius: Rational32 = get_req_form_r32(&req_kv, "temperature_celsius")?;
    if temp_celsius < *ABSOLUTE_ZERO_CELSIUS {
        // temperature below absolute zero?!
        return Err(ClientError::RationalValueTooLow("temperature_celsius".into(), temp_celsius, *ABSOLUTE_ZERO_CELSIUS));
    }

    let local_now = Local::now();
    let measurement = BodyTemperatureMeasurement::new(
        -1,
        local_now,
        location_id,
        temp_celsius,
    );
    Ok(measurement)
}

async fn get_sugar_measurement_from_form(req_kv: &HashMap<String, String>) -> Result<BloodSugarMeasurement, ClientError> {
    let unit_key = match req_kv.get("sugar_unit_key") {
        Some(uk) => uk,
        None => return Err(ClientError::MissingValue("sugar_unit_key".to_owned())),
    };
    let factor_to_mmol_per_l = if unit_key == "mmol-per-l" {
        Rational32::new(1, 1)
    } else if unit_key == "mg-per-dl" {
        Rational32::new(1, 18)
    } else {
        return Err(ClientError::ValueIsInvalidOption(
            "sugar_unit_key".to_owned(),
            unit_key.clone(),
            vec!["mmol-per-l".to_owned(), "mg-per-dl".to_owned()],
        ));
    };

    let sugar_value: Rational32 = get_req_form_r32_gt0(&req_kv, "sugar_value")?;
    let sugar_mmol_per_l: Rational32 = sugar_value * factor_to_mmol_per_l;

    let local_now = Local::now();
    let measurement = BloodSugarMeasurement::new(
        -1,
        local_now,
        sugar_mmol_per_l,
    );
    Ok(measurement)
}

async fn post_index(req: Request<Body>, token: &AuthToken) -> Result<Response<Body>, Infallible> {
    if !token.write {
        return respond_403_ro().await;
    }

    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = match body::to_bytes(req_body).await {
        Ok(rbb) => rbb,
        Err(e) => {
            error!("error reading request bytes: {}", e);
            return respond_500();
        },
    }.to_vec();
    let req_kv: HashMap<String, String> = form_urlencoded::parse(&req_body_bytes)
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();

    let new_measurement = match get_measurement_from_form(&req_kv) {
        Ok(nm) => nm,
        Err(e) => {
            return respond_400(e).await;
        },
    };

    match add_blood_pressure_measurement(&new_measurement).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error adding measurement: {}", e);
            return respond_500();
        },
    };

    redirect_to_self(req_parts).await
}

async fn post_mass(req: Request<Body>, token: &AuthToken) -> Result<Response<Body>, Infallible> {
    if !token.write {
        return respond_403_ro().await;
    }

    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = match body::to_bytes(req_body).await {
        Ok(rbb) => rbb,
        Err(e) => {
            error!("error reading request bytes: {}", e);
            return respond_500();
        },
    }.to_vec();
    let req_kv: HashMap<String, String> = form_urlencoded::parse(&req_body_bytes)
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();

    let new_measurement = match get_mass_measurement_from_form(&req_kv).await {
        Ok(nm) => nm,
        Err(e) => {
            return respond_400(e).await;
        },
    };

    match add_mass_measurement(&new_measurement).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error adding measurement: {}", e);
            return respond_500();
        },
    };

    redirect_to_self(req_parts).await
}

async fn post_temperature(req: Request<Body>, token: &AuthToken) -> Result<Response<Body>, Infallible> {
    if !token.write {
        return respond_403_ro().await;
    }

    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = match body::to_bytes(req_body).await {
        Ok(rbb) => rbb,
        Err(e) => {
            error!("error reading request bytes: {}", e);
            return respond_500();
        },
    }.to_vec();
    let req_kv: HashMap<String, String> = form_urlencoded::parse(&req_body_bytes)
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();

    let new_measurement = match get_temperature_measurement_from_form(&req_kv).await {
        Ok(nm) => nm,
        Err(e) => {
            return respond_400(e).await;
        },
    };

    match add_temperature_measurement(&new_measurement).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error adding measurement: {}", e);
            return respond_500();
        },
    };

    redirect_to_self(req_parts).await
}

async fn post_sugar(req: Request<Body>, token: &AuthToken) -> Result<Response<Body>, Infallible> {
    if !token.write {
        return respond_403_ro().await;
    }

    let (req_parts, req_body) = req.into_parts();
    let req_body_bytes = match body::to_bytes(req_body).await {
        Ok(rbb) => rbb,
        Err(e) => {
            error!("error reading request bytes: {}", e);
            return respond_500();
        },
    }.to_vec();
    let req_kv: HashMap<String, String> = form_urlencoded::parse(&req_body_bytes)
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();

    let new_measurement = match get_sugar_measurement_from_form(&req_kv).await {
        Ok(nm) => nm,
        Err(e) => {
            return respond_400(e).await;
        },
    };

    match add_blood_sugar_measurement(&new_measurement).await {
        Ok(rm) => rm,
        Err(e) => {
            error!("error adding measurement: {}", e);
            return respond_500();
        },
    };

    redirect_to_self(req_parts).await
}

async fn respond_static_file(file_name: &str) -> Result<Response<Body>, Infallible> {
    let mime_type = if file_name.ends_with(".css") {
        "text/css"
    } else if file_name.ends_with(".js") {
        "text/javascript"
    } else if file_name.ends_with(".jpg") || file_name.ends_with(".jpeg") {
        "image/jpeg"
    } else if file_name.ends_with(".png") {
        "image/png"
    } else if file_name.ends_with(".txt") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    };

    let buf = if file_name == "style.css" {
        Vec::from(&include_bytes!("../static/style.css")[..])
    } else if file_name == "beepee.js" {
        Vec::from(&include_bytes!("../static/beepee.js")[..])
    } else if file_name == "beepee.js.map" {
        Vec::from(&include_bytes!("../static/beepee.js.map")[..])
    } else if file_name == "beepee.ts" {
        Vec::from(&include_bytes!("../static/beepee.ts")[..])
    } else if file_name == "chart.js" {
        Vec::from(&include_bytes!("../static/chart.js")[..])
    } else if file_name == "chart.min.js" {
        Vec::from(&include_bytes!("../static/chart.min.js")[..])
    } else if file_name == "luxon.js" {
        Vec::from(&include_bytes!("../static/luxon.js")[..])
    } else if file_name == "chartjs-adapter-luxon.js" {
        Vec::from(&include_bytes!("../static/chartjs-adapter-luxon.js")[..])
    } else if file_name == "tsconfig.json" {
        Vec::from(&include_bytes!("../static/tsconfig.json")[..])
    } else {
        return respond_404().await;
    };

    let response_res = Response::builder()
        .header("Content-Length", format!("{}", buf.len()))
        .header("Content-Type", mime_type)
        .body(Body::from(buf));
    match response_res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("failed to create response: {}", e);
            return respond_500();
        }
    }
}

async fn handle_request(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    if let Some(cap) = STATIC_PATH_RE.captures(req.uri().path()) {
        let static_file_name = cap.get(1).expect("filename captured");
        return respond_static_file(static_file_name.as_str()).await;
    }

    // endpoints that do not require authentication before this line

    // check for token
    let query_str = match req.uri().query() {
        None => return respond_403().await,
        Some(q) => q,
    };
    let query_kv: HashMap<String, String> = form_urlencoded::parse(query_str.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let token_value = match query_kv.get("token") {
        None => return respond_403().await,
        Some(tv) => tv,
    };

    let token_opt = {
        CONFIG
            .get().expect("config is set")
            .read().await
            .auth_tokens
            .iter()
            .filter(|t| &t.token == token_value)
            .map(|t| t.clone())
            .nth(0)
    };
    let token = match token_opt {
        Some(t) => t,
        None => {
            // no such token found, at all
            return respond_403().await;
        },
    };

    // authenticated-only endpoints beyond this line

    if req.uri().path() == "/" {
        if req.method() == Method::GET {
            get_index(&token).await
        } else if req.method() == Method::POST {
            post_index(req, &token).await
        } else {
            respond_405(&[Method::GET, Method::POST]).await
        }
    } else if req.uri().path() == "/mass" {
        if req.method() == Method::GET {
            get_mass(&token).await
        } else if req.method() == Method::POST {
            post_mass(req, &token).await
        } else {
            respond_405(&[Method::GET, Method::POST]).await
        }
    } else if req.uri().path() == "/temperature" {
        if req.method() == Method::GET {
            get_temperature(&token).await
        } else if req.method() == Method::POST {
            post_temperature(req, &token).await
        } else {
            respond_405(&[Method::GET, Method::POST]).await
        }
    } else if req.uri().path() == "/sugar" {
        if req.method() == Method::GET {
            get_sugar(&token).await
        } else if req.method() == Method::POST {
            post_sugar(req, &token).await
        } else {
            respond_405(&[Method::GET, Method::POST]).await
        }
    } else if req.uri().path() == "/api/bp" {
        if req.method() == Method::GET {
            get_api_bp().await
        } else {
            respond_405(&[Method::GET]).await
        }
    } else if req.uri().path() == "/api/mass" {
        if req.method() == Method::GET {
            get_api_mass().await
        } else {
            respond_405(&[Method::GET]).await
        }
    } else if req.uri().path() == "/api/temperature" {
        if req.method() == Method::GET {
            get_api_temperature().await
        } else {
            respond_405(&[Method::GET]).await
        }
    } else if req.uri().path() == "/api/sugar" {
        if req.method() == Method::GET {
            get_api_sugar().await
        } else {
            respond_405(&[Method::GET]).await
        }
    } else {
        respond_404().await
    }
}

async fn run() -> Result<(), ServerError> {
    env_logger::init();

    let args: Vec<OsString> = std::env::args_os().collect();
    let config_path = match args.get(1) {
        Some(cp) => PathBuf::from(cp),
        None => PathBuf::from("config.toml"),
    };
    CONFIG_PATH
        .set(config_path).expect("failed to set config path");

    load_config().await?;

    let mut tera = Tera::default();
    tera.autoescape_on(vec![]);
    tera.register_filter("ratio2float", RatioToFloat);
    tera.add_raw_templates(vec![
        ("400.html.tera", include_str!("../templates/400.html.tera")),
        ("403.html.tera", include_str!("../templates/403.html.tera")),
        ("403_ro.html.tera", include_str!("../templates/403_ro.html.tera")),
        ("404.html.tera", include_str!("../templates/404.html.tera")),
        ("405.html.tera", include_str!("../templates/405.html.tera")),
        ("base.html.tera", include_str!("../templates/base.html.tera")),
        ("list_macros.tera", include_str!("../templates/list_macros.tera")),
        ("list.html.tera", include_str!("../templates/list.html.tera")),
        ("mass_list.html.tera", include_str!("../templates/mass_list.html.tera")),
        ("redirect.html.tera", include_str!("../templates/redirect.html.tera")),
        ("sugar_list.html.tera", include_str!("../templates/sugar_list.html.tera")),
        ("temperature_list.html.tera", include_str!("../templates/temperature_list.html.tera")),
    ])
        .map_err(|e| ServerError::TemplatingSetup(e))?;
    TERA
        .set(RwLock::new(tera)).expect("failed to set templating engine");

    let addr: SocketAddr = {
        CONFIG
            .get().expect("no config lock")
            .read().await
            .http_listen
            .parse()
            .map_err(|e| ServerError::ParsingListenAddress(e))?
    };

    let make_service = make_service_fn(|_conn| async {
        Ok::<_, Infallible>(service_fn(handle_request))
    });

    let server = Server::bind(&addr).serve(make_service);
    server.await
        .map_err(|e| ServerError::HyperError(e))
}

fn main() {
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            run().await
        });

    std::process::exit(match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{}", e);
            1
        },
    });
}
