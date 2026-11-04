use super::*;
use base64::Engine as _;
use rstest::rstest;
use std::sync::LazyLock;

pub(crate) struct Key {
    der: Vec<u8>,
    jwk: serde_json::Value,
}

pub(crate) fn rsa_key(kid: &str) -> Key {
    use rsa::pkcs1::EncodeRsaPrivateKey as _;
    use rsa::traits::PublicKeyParts as _;
    let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Key {
        der: key.to_pkcs1_der().unwrap().as_bytes().to_vec(),
        jwk: serde_json::json!({
            "kty": "RSA", "use": "sig", "alg": "RS256", "kid": kid,
            "n": b64.encode(key.n().to_bytes_be()),
            "e": b64.encode(key.e().to_bytes_be()),
        }),
    }
}

pub(crate) static CLUSTER: LazyLock<Key> = LazyLock::new(|| rsa_key("k1"));
static ROTATED: LazyLock<Key> = LazyLock::new(|| rsa_key("k2"));
static FORGER: LazyLock<Key> = LazyLock::new(|| rsa_key("k1"));

pub(crate) fn jwks(keys: &[&Key]) -> String {
    serde_json::json!({ "keys": keys.iter().map(|k| k.jwk.clone()).collect::<Vec<_>>() })
        .to_string()
}

pub(crate) fn nomad_claims(job: &str) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    serde_json::json!({
        "aud": AUDIENCE,
        "iss": "https://nomad.home",
        "sub": format!("global:default:{job}:executor:agent:buckle"),
        "nomad_namespace": "default",
        "nomad_job_id": job,
        "iat": now,
        "exp": now + 3600,
    })
}

pub(crate) fn mint(key: &Key, kid: &str, claims: &serde_json::Value) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(kid.into());
    jsonwebtoken::encode(
        &header,
        claims,
        &jsonwebtoken::EncodingKey::from_rsa_der(&key.der),
    )
    .unwrap()
}

fn pinned(cluster: &str, keys: &[&Key]) -> Candidate {
    Candidate {
        cluster: cluster.into(),
        kind: ClusterKind::Nomad,
        trust: ClusterTrust {
            keys: Some(jwks(keys)),
            ..Default::default()
        },
        learned: None,
    }
}

fn with(claims: serde_json::Value, edit: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
    let mut claims = claims;
    edit(&mut claims);
    claims
}

#[tokio::test]
async fn the_default_executor_job_is_accepted_for_its_cluster() {
    let token = mint(&CLUSTER, "k1", &nomad_claims("buckle-executor"));
    let verified = WorkloadTrust::default()
        .verify(&[pinned("home-nomad", &[&CLUSTER])], &token, None)
        .await
        .unwrap();
    assert_eq!(verified.cluster, "home-nomad");
    assert_eq!(verified.subject, "default/buckle-executor");
}

#[rstest]
#[case::another_job(nomad_claims("paste"), &*CLUSTER, "not one of its trusted subjects")]
#[case::another_audience(with(nomad_claims("buckle-executor"), |c| c["aud"] = "vault".into()), &*CLUSTER, "not meant for")]
#[case::expired(with(nomad_claims("buckle-executor"), |c| c["exp"] = (chrono::Utc::now().timestamp() - 600).into()), &*CLUSTER, "expired")]
#[case::forged(nomad_claims("buckle-executor"), &*FORGER, "do not verify")]
#[tokio::test]
async fn tokens_the_cluster_did_not_mean_for_its_executor_are_refused(
    #[case] claims: serde_json::Value,
    #[case] signer: &Key,
    #[case] reason: &str,
) {
    let token = mint(signer, "k1", &claims);
    let err = WorkloadTrust::default()
        .verify(&[pinned("home-nomad", &[&CLUSTER])], &token, None)
        .await
        .unwrap_err();
    assert!(err.contains(reason), "{err}");
}

#[tokio::test]
async fn an_issuer_and_subjects_narrow_what_is_accepted() {
    let mut c = pinned("home-nomad", &[&CLUSTER]);
    c.trust.issuer = Some("https://other".into());
    c.trust.subjects = vec!["ops/runner".into()];
    let token = mint(&CLUSTER, "k1", &nomad_claims("buckle-executor"));
    let err = WorkloadTrust::default()
        .verify(&[c.clone()], &token, None)
        .await
        .unwrap_err();
    assert!(err.contains("issuer"), "{err}");

    c.trust.issuer = Some("https://nomad.home".into());
    let ok = mint(
        &CLUSTER,
        "k1",
        &with(nomad_claims("runner"), |v| {
            v["nomad_namespace"] = "ops".into()
        }),
    );
    let verified = WorkloadTrust::default()
        .verify(&[c], &ok, None)
        .await
        .unwrap();
    assert_eq!(verified.subject, "ops/runner");
}

#[rstest]
#[case::symmetric({
    let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    h.kid = Some("k1".into());
    jsonwebtoken::encode(&h, &nomad_claims("buckle-executor"), &jsonwebtoken::EncodingKey::from_secret(b"k")).unwrap()
}, "asymmetric")]
#[case::no_kid({
    jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &nomad_claims("buckle-executor"), &jsonwebtoken::EncodingKey::from_rsa_der(&CLUSTER.der)).unwrap()
}, "`kid`")]
#[case::not_a_jwt("hello".to_owned(), "not a JWT")]
#[tokio::test]
async fn malformed_tokens_are_refused(#[case] token: String, #[case] reason: &str) {
    let err = WorkloadTrust::default()
        .verify(&[pinned("home-nomad", &[&CLUSTER])], &token, None)
        .await
        .unwrap_err();
    assert!(err.contains(reason), "{err}");
}

#[tokio::test]
async fn keys_are_fetched_from_the_clusters_jwks_url() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::path("/.well-known/jwks.json"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_raw(jwks(&[&CLUSTER]), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let c = Candidate {
        cluster: "home-nomad".into(),
        kind: ClusterKind::Nomad,
        trust: ClusterTrust {
            jwks_url: Some(format!("{}/.well-known/jwks.json", server.uri())),
            ..Default::default()
        },
        learned: None,
    };
    let trust = WorkloadTrust::default();
    let token = mint(&CLUSTER, "k1", &nomad_claims("buckle-executor"));
    for _ in 0..2 {
        trust
            .verify(std::slice::from_ref(&c), &token, None)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_relayed_key_set_is_learned_only_when_it_holds_the_trusted_key() {
    let trust = WorkloadTrust::default();
    let c = pinned("home-nomad", &[&CLUSTER]);
    let token = mint(&CLUSTER, "k1", &nomad_claims("buckle-executor"));

    let forged = jwks(&[&FORGER, &ROTATED]);
    let refused = trust
        .verify(std::slice::from_ref(&c), &token, Some(&forged))
        .await
        .unwrap();
    assert_eq!(
        refused.learn, None,
        "a relay holding another key under the same kid"
    );

    let rotated = jwks(&[&CLUSTER, &ROTATED]);
    let learned = trust
        .verify(std::slice::from_ref(&c), &token, Some(&rotated))
        .await
        .unwrap()
        .learn
        .expect("the rotated set is learned");

    // After rotation the cluster signs with the new key the pinned set lacks.
    let after = mint(&ROTATED, "k2", &nomad_claims("buckle-executor"));
    assert!(
        trust
            .verify(std::slice::from_ref(&c), &after, None)
            .await
            .is_err()
    );
    let c = Candidate {
        learned: Some(learned.clone()),
        ..c
    };
    let again = trust.verify(&[c], &after, Some(&learned)).await.unwrap();
    assert_eq!(again.learn, None, "nothing new to learn");
}

#[tokio::test]
async fn a_token_two_clusters_accept_is_refused() {
    let token = mint(&CLUSTER, "k1", &nomad_claims("buckle-executor"));
    let err = WorkloadTrust::default()
        .verify(
            &[pinned("a", &[&CLUSTER]), pinned("b", &[&CLUSTER])],
            &token,
            None,
        )
        .await
        .unwrap_err();
    assert!(err.contains("several clusters"), "{err}");
}

#[test]
fn kubernetes_tokens_are_named_by_their_subject() {
    let claims = serde_json::json!({ "sub": "system:serviceaccount:buckle-system:buckle-executor", "nomad_job_id": "x" });
    let claims = claims.as_object().unwrap();
    assert_eq!(
        subject(ClusterKind::Kubernetes, claims).as_deref(),
        Some("system:serviceaccount:buckle-system:buckle-executor")
    );
}
