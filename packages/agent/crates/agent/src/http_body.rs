//! Reads an HTTP answer's body up to a limit, so a service that answers with too much cannot fill
//! the agent's memory.

#[must_use]
pub(crate) fn is_success(status: u16) -> bool {
    reqwest::StatusCode::from_u16(status).is_ok_and(|status| status.is_success())
}

/// The most an answer from steamcmd.net, Steam's Web API or an image registry may carry. Their
/// real answers are a few tens of kilobytes.
pub(crate) const ANSWER_MAX_BYTES: usize = 1024 * 1024;

/// `None` when the body carries more than `limit` bytes. The read stops at the first chunk past it.
///
/// # Errors
///
/// Fails as reading the body fails.
pub(crate) async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body = Vec::new();

    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }

        body.extend_from_slice(&chunk);
    }

    Ok(Some(body))
}

/// The error text every capped client uses for an answer over the limit.
pub(crate) fn too_large(limit: usize) -> String {
    format!("the answer is larger than {limit} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(body: &'static [u8]) -> reqwest::Response {
        reqwest::Response::from(hyper::Response::new(body))
    }

    #[tokio::test]
    async fn reads_a_body_up_to_the_limit_and_refuses_one_past_it() {
        assert_eq!(read_limited(answer(b"12345"), 5).await.unwrap().as_deref(), Some(&b"12345"[..]));
        assert_eq!(read_limited(answer(b"123456"), 5).await.unwrap(), None);
        assert_eq!(too_large(5), "the answer is larger than 5 bytes");
    }

    #[tokio::test]
    async fn takes_answers_of_up_to_a_mebibyte() {
        let exact = reqwest::Response::from(hyper::Response::new(vec![b'x'; 1024 * 1024]));
        let long = reqwest::Response::from(hyper::Response::new(vec![b'x'; 1024 * 1024 + 1]));

        assert_eq!(read_limited(exact, ANSWER_MAX_BYTES).await.unwrap().map(|body| body.len()), Some(1024 * 1024));
        assert_eq!(read_limited(long, ANSWER_MAX_BYTES).await.unwrap(), None);
    }
}
