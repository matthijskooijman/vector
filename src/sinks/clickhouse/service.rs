//! Service implementation for the `Clickhouse` sink.

use super::sink::PartitionKey;
use crate::{
    http::{Auth, HttpError},
    sinks::{
        prelude::*,
        util::{
            http::{HttpRequest, HttpResponse, HttpRetryLogic, HttpServiceRequestBuilder},
            retries::RetryAction,
        },
        UriParseSnafu,
    },
};
use bytes::Bytes;
use http::{
    header::{CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE},
    Request, StatusCode, Uri,
};
use snafu::ResultExt;

#[derive(Debug, Default, Clone)]
pub struct ClickhouseRetryLogic {
    inner: HttpRetryLogic,
}

impl RetryLogic for ClickhouseRetryLogic {
    type Error = HttpError;
    type Response = HttpResponse;

    fn is_retriable_error(&self, error: &Self::Error) -> bool {
        self.inner.is_retriable_error(error)
    }

    fn should_retry_response(&self, response: &Self::Response) -> RetryAction {
        match response.http_response.status() {
            StatusCode::INTERNAL_SERVER_ERROR => {
                let body = response.http_response.body();

                // Currently, ClickHouse returns 500's incorrect data and type mismatch errors.
                // This attempts to check if the body starts with `Code: {code_num}` and to not
                // retry those errors.
                //
                // Reference: https://github.com/vectordotdev/vector/pull/693#issuecomment-517332654
                // Error code definitions: https://github.com/ClickHouse/ClickHouse/blob/master/dbms/src/Common/ErrorCodes.cpp
                //
                // Fix already merged: https://github.com/ClickHouse/ClickHouse/pull/6271
                if body.starts_with(b"Code: 117") {
                    RetryAction::DontRetry("incorrect data".into())
                } else if body.starts_with(b"Code: 53") {
                    RetryAction::DontRetry("type mismatch".into())
                } else {
                    RetryAction::Retry(String::from_utf8_lossy(body).to_string().into())
                }
            }
            _ => self.inner.should_retry_response(&response.http_response),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ClickhouseServiceRequestBuilder {
    pub(super) auth: Option<Auth>,
    pub(super) endpoint: Uri,
    pub(super) skip_unknown_fields: bool,
    pub(super) date_time_best_effort: bool,
    pub(super) compression: Compression,
}

impl HttpServiceRequestBuilder<PartitionKey> for ClickhouseServiceRequestBuilder {
    fn build(&self, request: HttpRequest<PartitionKey>) -> Request<Bytes> {
        let metadata = request
            .get_additional_metadata()
            .expect("PartitionKey should have been set upstream");

        let uri = set_uri_query(
            &self.endpoint,
            &metadata.database,
            &metadata.table,
            self.skip_unknown_fields,
            self.date_time_best_effort,
        )
        .expect("building uri failed unexpectedly");

        let auth: Option<Auth> = self.auth.clone();

        let mut builder = Request::post(&uri)
            .header(CONTENT_TYPE, "application/x-ndjson")
            .header(CONTENT_LENGTH, request.get_payload().len());
        if let Some(ce) = self.compression.content_encoding() {
            builder = builder.header(CONTENT_ENCODING, ce);
        }
        if let Some(auth) = auth {
            builder = auth.apply_builder(builder);
        }

        builder
            .body(request.get_payload().clone())
            .expect("building HTTP request failed unexpectedly")
    }
}

fn set_uri_query(
    uri: &Uri,
    database: &str,
    table: &str,
    skip_unknown: bool,
    date_time_best_effort: bool,
) -> crate::Result<Uri> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "query",
            format!(
                "INSERT INTO \"{}\".\"{}\" FORMAT JSONEachRow",
                database,
                table.replace('\"', "\\\"")
            )
            .as_str(),
        )
        .finish();

    let mut uri = uri.to_string();
    if !uri.ends_with('/') {
        uri.push('/');
    }

    uri.push_str("?input_format_import_nested_json=1&");
    if skip_unknown {
        uri.push_str("input_format_skip_unknown_fields=1&");
    }
    if date_time_best_effort {
        uri.push_str("date_time_input_format=best_effort&")
    }
    uri.push_str(query.as_str());

    uri.parse::<Uri>()
        .context(UriParseSnafu)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_valid() {
        let uri = set_uri_query(
            &"http://localhost:80".parse().unwrap(),
            "my_database",
            "my_table",
            false,
            true,
        )
        .unwrap();
        assert_eq!(uri.to_string(), "http://localhost:80/?input_format_import_nested_json=1&date_time_input_format=best_effort&query=INSERT+INTO+%22my_database%22.%22my_table%22+FORMAT+JSONEachRow");

        let uri = set_uri_query(
            &"http://localhost:80".parse().unwrap(),
            "my_database",
            "my_\"table\"",
            false,
            false,
        )
        .unwrap();
        assert_eq!(uri.to_string(), "http://localhost:80/?input_format_import_nested_json=1&query=INSERT+INTO+%22my_database%22.%22my_%5C%22table%5C%22%22+FORMAT+JSONEachRow");
    }

    #[test]
    fn encode_invalid() {
        set_uri_query(
            &"localhost:80".parse().unwrap(),
            "my_database",
            "my_table",
            false,
            false,
        )
        .unwrap_err();
    }
}
