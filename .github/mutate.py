"""Throwaway: undo one fix so the live Keycloak test that guards it can be seen red."""
import sys
p = "orchestrator/core/src/infrastructure/iam/keycloak_admin_client.rs"
s = open(p).read()
def rep(old, new):
    global s
    assert s.count(old) == 1, old
    s = s.replace(old, new)
m = sys.argv[1]
if m == "policy":
    rep('"passwordPolicy": "length(12) and maxLength(128) and notUsername and notEmail",',
        '"passwordPolicy": "length(12) and notARealPolicy(3)",')
elif m == "enabled":
    rep("    if let Some(enabled) = user.enabled {", "    if let Some(enabled) = user.enabled.map(|_| true) {")
elif m == "lock":
    rep("        let _one_at_a_time = self.lock_user(realm, user_id).await?;", "")
elif m == "invite":
    rep("""            None => self.create_invited_user(&realm, email).await?,
        };
""", """            None => self.create_invited_user(&realm, email).await?,
        };
        {
            let token = self.get_admin_token().await?;
            let resp = self
                .http
                .put(format!("{}/admin/realms/{}/users/{}", self.config.host, realm, user_id))
                .bearer_auth(&token)
                .json(&serde_json::json!({"attributes": {"aegis_role": ["member"], "team_slug": [team_slug]}}))
                .send()
                .await?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                return Err(KeycloakAdminError::AttributeError { status, body });
            }
        }
""")
elif m == "saml":
    rep('                "validateSignature": "true",', '                "validateSignature": "false",')
    rep("        check_signing_certificate(&config.certificate)?;", "")
else:
    sys.exit("unknown mutation")
open(p, "w").write(s)
