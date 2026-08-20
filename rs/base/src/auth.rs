use std::sync::Arc;

/// The carriers RFC 8d.1 names. `Http`, `Ws` and `Sse` are the non-local serve
/// routes this one function guards; `Uds` and `Gateway` are the local carriers
/// whose authorizer is P3's peer-cred / connection-to-env check, recorded here
/// so the whole set lives in one enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    Uds,
    Gateway,
    Http,
    Ws,
    Sse,
}

/// One serve request as the authorizer reads it: the bearer token the client
/// presented (from `Authorization: Bearer` or `?token=`) and whether the target
/// app is explicitly `public` (ingress is P4.5; the flag is supported here).
pub struct Request<'a> {
    pub token: Option<&'a str>,
    pub public: bool,
}

/// What a passed check grants. For serve carriers in P4.4 that is only "this
/// connection is authenticated"; env binding (auth.scope) stays P3's and
/// per-app scoping arrives with ingress (P4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub carrier: Carrier,
    pub env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reject {
    MissingToken,
    BadToken,
    NotConfigured,
}

impl Reject {
    pub fn message(&self) -> &'static str {
        match self {
            Reject::MissingToken => "missing bearer token",
            Reject::BadToken => "invalid bearer token",
            Reject::NotConfigured => "serve requires --auth-token or --public",
        }
    }
}

/// The bearer configuration a serve process starts with: the expected token
/// (from `--auth-token` or `TENON_AUTH_TOKEN`) and whether this serve surface is
/// public. Shared by the HTTP, WS and SSE handlers so all three read one value.
#[derive(Debug, Clone, Default)]
pub struct Auth {
    token: Option<Arc<String>>,
    public: bool,
}

impl Auth {
    pub fn new(token: Option<String>, public: bool) -> Auth {
        Auth {
            token: token.filter(|t| !t.is_empty()).map(Arc::new),
            public,
        }
    }

    /// Reads the token from the CLI flag, then the `TENON_AUTH_TOKEN` env var.
    pub fn resolve(flag: Option<String>, public: bool) -> Auth {
        let token = flag.or_else(|| std::env::var("TENON_AUTH_TOKEN").ok());
        Auth::new(token, public)
    }

    pub fn is_public(&self) -> bool {
        self.public
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub fn expected(&self) -> Option<&str> {
        self.token.as_deref().map(String::as_str)
    }
}

/// The single gate for every serve carrier (RFC 8d.1): the bearer-token check
/// lives here and nowhere else, so adding a route never adds an auth path. A
/// public target skips the token; otherwise the presented token must be present
/// and equal to the configured one under a constant-time compare.
pub fn authorize(carrier: Carrier, request: &Request, auth: &Auth) -> Result<Scope, Reject> {
    if auth.is_public() || request.public {
        return Ok(Scope { carrier, env: None });
    }
    let Some(expected) = auth.expected() else {
        return Err(Reject::NotConfigured);
    };
    let Some(presented) = request.token else {
        return Err(Reject::MissingToken);
    };
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        Ok(Scope { carrier, env: None })
    } else {
        Err(Reject::BadToken)
    }
}

/// A length-independent, value-independent comparison: it always walks the
/// longer of the two inputs and folds every byte into one accumulator, so the
/// time it takes never reveals how much of the token was right.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let len = a.len().max(b.len());
    for index in 0..len {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= left ^ right;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> Auth {
        Auth::new(Some("s3cr3t".to_string()), false)
    }

    #[test]
    fn a_public_target_needs_no_token() {
        let auth = Auth::new(None, true);
        let request = Request {
            token: None,
            public: false,
        };
        assert!(authorize(Carrier::Http, &request, &auth).is_ok());
    }

    #[test]
    fn the_right_token_passes_the_wrong_one_and_none_fail() {
        let ok = Request {
            token: Some("s3cr3t"),
            public: false,
        };
        assert!(authorize(Carrier::Ws, &ok, &auth()).is_ok());
        let bad = Request {
            token: Some("nope"),
            public: false,
        };
        assert_eq!(authorize(Carrier::Ws, &bad, &auth()), Err(Reject::BadToken));
        let none = Request {
            token: None,
            public: false,
        };
        assert_eq!(
            authorize(Carrier::Http, &none, &auth()),
            Err(Reject::MissingToken)
        );
    }

    #[test]
    fn no_token_and_not_public_is_a_misconfiguration() {
        let auth = Auth::new(None, false);
        let request = Request {
            token: Some("anything"),
            public: false,
        };
        assert_eq!(
            authorize(Carrier::Http, &request, &auth),
            Err(Reject::NotConfigured)
        );
    }

    #[test]
    fn constant_time_eq_matches_only_equal_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }
}
