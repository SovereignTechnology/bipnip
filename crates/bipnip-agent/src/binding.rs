//! Creating and revoking the node ↔ Nostr key binding (`docs/nips/node-binding.md`).
//!
//! The agent asks its own node who it is, has the node sign the canonical statement with its
//! `signmessage`, signs the event with the Nostr key, and then checks the finished event exactly as
//! a stranger would before handing it out. A binding that does not verify never leaves the agent.

use std::str::FromStr;

use bipnip_ln::{BackendError, LightningBackend};
use mostro_core::node_binding::{
    Action, BindingError, Network, SignedStatement, Statement, hex_signature_to_zbase32,
};
use nostr_sdk::prelude::{Event, FinalizeEvent, Keys};

/// Why a binding could not be made.
#[derive(Debug)]
pub enum BindError {
    /// The node could not be asked, or refused.
    Node(BackendError),
    /// The node answered with something unusable (unknown network, malformed key).
    NodeAnswer(String),
    /// The node is not on the network the caller expects (a regtest node is never bound as mainnet).
    WrongNetwork {
        /// The network the caller asked for.
        expected: Network,
        /// What the node reports.
        node: Network,
    },
    /// The statement, signature or event failed the same checks verifiers run.
    Binding(BindingError),
    /// The Nostr key could not sign the event.
    Sign(String),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::Node(e) => write!(f, "{e}"),
            BindError::NodeAnswer(e) => write!(f, "unusable answer from the node: {e}"),
            BindError::WrongNetwork { expected, node } => write!(
                f,
                "the node is on {}, not {}",
                node.as_str(),
                expected.as_str()
            ),
            BindError::Binding(e) => write!(f, "{e}"),
            BindError::Sign(e) => write!(f, "signing the binding event: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

/// Builds a signed, verified binding (or revocation) event for `keys` and the node behind
/// `backend`, dated `now` (unix seconds). The node must be on `network`.
pub async fn make_binding_event(
    backend: &dyn LightningBackend,
    keys: &Keys,
    action: Action,
    network: Network,
    now: u64,
) -> Result<Event, BindError> {
    let info = backend.node_info().await.map_err(BindError::Node)?;
    let node_network = Network::from_str(&info.network)
        .map_err(|_| BindError::NodeAnswer(format!("network {:?}", info.network)))?;
    if node_network != network {
        return Err(BindError::WrongNetwork {
            expected: network,
            node: node_network,
        });
    }
    let node_pubkey = secp256k1::PublicKey::from_str(&info.node_pubkey)
        .map_err(|_| BindError::NodeAnswer("node public key".into()))?;
    let statement = Statement::new(
        action,
        network,
        &keys.public_key().to_hex(),
        node_pubkey,
        now,
    )
    .map_err(BindError::Binding)?;

    let raw = backend
        .sign_message(statement.text().as_bytes())
        .await
        .map_err(BindError::Node)?;
    // Eclair answers with the same 65 bytes in hex; everything else in zbase32.
    let node_signature = if raw.len() == 130 {
        hex_signature_to_zbase32(&raw).map_err(BindError::Binding)?
    } else {
        raw
    };
    let signed = SignedStatement {
        statement,
        node_signature,
    };
    // The node must have signed with the key it reported: catch a misbehaving backend here.
    signed.verify().map_err(BindError::Binding)?;

    let event = signed
        .to_event_builder()
        .finalize(keys)
        .map_err(|e| BindError::Sign(e.to_string()))?;
    // Last gate: the finished event passes every check a verifier applies.
    let checked = SignedStatement::from_event(&event).map_err(BindError::Binding)?;
    if checked != signed {
        return Err(BindError::Binding(BindingError::Event(
            "event does not carry the statement that was signed",
        )));
    }
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bipnip_ln::{
        ChannelView, HoldInvoiceRequest, HoldState, NodeInfo, OpenRequest, PaymentState,
    };
    use mostro_core::node_binding::{is_bound, sign_ln_message, zbase32};
    use secp256k1::{Secp256k1, SecretKey};

    /// A node that signs with `secret`, reports `reported` as its key, and can answer in hex.
    struct FakeNode {
        secret: SecretKey,
        reported: secp256k1::PublicKey,
        network: &'static str,
        hex: bool,
    }

    fn key(n: u8) -> SecretKey {
        let mut b = [0u8; 32];
        b[31] = n;
        SecretKey::from_slice(&b).expect("key")
    }

    fn public(sk: &SecretKey) -> secp256k1::PublicKey {
        secp256k1::PublicKey::from_secret_key(&Secp256k1::signing_only(), sk)
    }

    fn node(n: u8) -> FakeNode {
        FakeNode {
            secret: key(n),
            reported: public(&key(n)),
            network: "regtest",
            hex: false,
        }
    }

    fn unsupported<T>() -> Result<T, BackendError> {
        Err(BackendError::Unsupported("not needed for binding"))
    }

    #[async_trait]
    impl LightningBackend for FakeNode {
        async fn node_info(&self) -> Result<NodeInfo, BackendError> {
            Ok(NodeInfo {
                node_pubkey: self.reported.to_string(),
                network: self.network.to_string(),
                block_height: 1,
                synced: true,
            })
        }
        async fn sign_message(&self, msg: &[u8]) -> Result<String, BackendError> {
            let z = sign_ln_message(msg, &self.secret);
            if self.hex {
                let bytes = zbase32::decode(&z).expect("zbase32");
                Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
            } else {
                Ok(z)
            }
        }
        async fn add_hold_invoice(&self, _: &HoldInvoiceRequest) -> Result<String, BackendError> {
            unsupported()
        }
        async fn hold_invoice_state(&self, _: &str) -> Result<HoldState, BackendError> {
            unsupported()
        }
        async fn settle_hold_invoice(&self, _: &str) -> Result<(), BackendError> {
            unsupported()
        }
        async fn cancel_hold_invoice(&self, _: &str) -> Result<(), BackendError> {
            unsupported()
        }
        async fn pay_invoice(&self, _: &str, _: u64, _: u32) -> Result<PaymentState, BackendError> {
            unsupported()
        }
        async fn payment_state(&self, _: &str) -> Result<PaymentState, BackendError> {
            unsupported()
        }
        async fn add_invoice(&self, _: u64, _: u32, _: &str) -> Result<String, BackendError> {
            unsupported()
        }
        async fn connect(&self, _: &str, _: &str) -> Result<(), BackendError> {
            unsupported()
        }
        async fn open_channel(&self, _: &OpenRequest) -> Result<String, BackendError> {
            unsupported()
        }
        async fn channels_with(&self, _: &str) -> Result<Vec<ChannelView>, BackendError> {
            unsupported()
        }
        async fn graph_node(&self, _: &str) -> Result<Option<(u32, u64)>, BackendError> {
            unsupported()
        }
    }

    #[tokio::test]
    async fn binds_and_a_stranger_can_verify() {
        let keys = Keys::generate();
        let n = node(1);
        let event = make_binding_event(&n, &keys, Action::Bind, Network::Regtest, 1_000)
            .await
            .expect("binding");
        let seen = SignedStatement::from_event(&event).expect("verifies");
        assert!(is_bound(
            &n.reported,
            &keys.public_key().to_hex(),
            Network::Regtest,
            [&seen],
            1_000
        ));
    }

    #[tokio::test]
    async fn revocation_beats_the_earlier_binding() {
        let keys = Keys::generate();
        let n = node(1);
        let bind = make_binding_event(&n, &keys, Action::Bind, Network::Regtest, 1_000)
            .await
            .expect("binding");
        let revoke = make_binding_event(&n, &keys, Action::Revoke, Network::Regtest, 2_000)
            .await
            .expect("revocation");
        let (b, r) = (
            SignedStatement::from_event(&bind).expect("bind"),
            SignedStatement::from_event(&revoke).expect("revoke"),
        );
        assert!(!is_bound(
            &n.reported,
            &keys.public_key().to_hex(),
            Network::Regtest,
            [&b, &r],
            2_000
        ));
    }

    #[tokio::test]
    async fn hex_signatures_are_accepted() {
        let keys = Keys::generate();
        let mut n = node(2);
        n.hex = true;
        let event = make_binding_event(&n, &keys, Action::Bind, Network::Regtest, 1_000)
            .await
            .expect("binding");
        assert!(SignedStatement::from_event(&event).is_ok());
    }

    #[tokio::test]
    async fn a_node_on_another_network_is_refused() {
        let keys = Keys::generate();
        let n = node(1);
        let err = make_binding_event(&n, &keys, Action::Bind, Network::Bitcoin, 1_000)
            .await
            .expect_err("wrong network");
        assert!(matches!(err, BindError::WrongNetwork { .. }), "{err}");
    }

    #[tokio::test]
    async fn a_node_that_signs_with_another_key_is_refused() {
        let keys = Keys::generate();
        let mut n = node(1);
        n.reported = public(&key(9));
        let err = make_binding_event(&n, &keys, Action::Bind, Network::Regtest, 1_000)
            .await
            .expect_err("wrong signer");
        assert!(
            matches!(err, BindError::Binding(BindingError::WrongSigner)),
            "{err}"
        );
    }
}
