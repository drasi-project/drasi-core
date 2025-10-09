// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub struct ScramClient {
    username: String,
    password: String,
    client_nonce: String,
    server_nonce: Option<String>,
    salt: Option<Vec<u8>>,
    iterations: Option<u32>,
    auth_message: Option<String>,
}

impl ScramClient {
    pub fn new(username: &str, password: &str) -> Self {
        let client_nonce = generate_nonce();
        Self {
            username: username.to_string(),
            password: password.to_string(),
            client_nonce,
            server_nonce: None,
            salt: None,
            iterations: None,
            auth_message: None,
        }
    }

    pub fn client_first_message(&self) -> String {
        // SCRAM client-first-message format: n,,n=<username>,r=<nonce>
        let gs2_header = "n,,"; // No channel binding
        let client_first_bare = format!("n={},r={}", saslprep(&self.username), self.client_nonce);
        format!("{}{}", gs2_header, client_first_bare)
    }

    pub fn process_server_first_message(&mut self, message: &str) -> Result<()> {
        // Parse server-first-message: r=<nonce>,s=<salt>,i=<iteration-count>
        let params = parse_scram_message(message)?;

        // Verify server nonce starts with client nonce
        let server_nonce = params
            .get("r")
            .ok_or_else(|| anyhow!("Missing nonce in server response"))?;
        if !server_nonce.starts_with(&self.client_nonce) {
            return Err(anyhow!("Server nonce doesn't include client nonce"));
        }
        self.server_nonce = Some(server_nonce.clone());

        // Parse salt
        let salt_b64 = params
            .get("s")
            .ok_or_else(|| anyhow!("Missing salt in server response"))?;
        self.salt = Some(BASE64.decode(salt_b64)?);

        // Parse iteration count
        let iterations_str = params
            .get("i")
            .ok_or_else(|| anyhow!("Missing iteration count in server response"))?;
        self.iterations = Some(iterations_str.parse()?);

        Ok(())
    }

    pub fn client_final_message(&mut self) -> Result<String> {
        let server_nonce = self
            .server_nonce
            .as_ref()
            .ok_or_else(|| anyhow!("Server nonce not set"))?;
        let salt = self.salt.as_ref().ok_or_else(|| anyhow!("Salt not set"))?;
        let iterations = self
            .iterations
            .ok_or_else(|| anyhow!("Iterations not set"))?;

        // Build client-final-message-without-proof
        let channel_binding = "c=biws"; // base64("n,,")
        let client_final_without_proof = format!("{},r={}", channel_binding, server_nonce);

        // Build auth message
        let client_first_bare = format!("n={},r={}", saslprep(&self.username), self.client_nonce);
        let server_first = format!(
            "r={},s={},i={}",
            server_nonce,
            BASE64.encode(salt),
            iterations
        );
        let auth_message = format!(
            "{},{},{}",
            client_first_bare, server_first, client_final_without_proof
        );
        self.auth_message = Some(auth_message.clone());

        // Calculate proof
        let salted_password = pbkdf2_sha256(self.password.as_bytes(), salt, iterations);
        let client_key = hmac_sha256(&salted_password, b"Client Key");
        let stored_key = sha256(&client_key);
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let client_proof = xor_bytes(&client_key, &client_signature);

        // Build final message
        Ok(format!(
            "{},p={}",
            client_final_without_proof,
            BASE64.encode(client_proof)
        ))
    }

    pub fn verify_server_final(&self, message: &str) -> Result<()> {
        let params = parse_scram_message(message)?;

        // Check for error
        if let Some(error) = params.get("e") {
            return Err(anyhow!("Server error: {}", error));
        }

        // Verify server signature
        if let Some(server_sig_b64) = params.get("v") {
            let auth_message = self
                .auth_message
                .as_ref()
                .ok_or_else(|| anyhow!("Auth message not set"))?;
            let salt = self.salt.as_ref().ok_or_else(|| anyhow!("Salt not set"))?;
            let iterations = self
                .iterations
                .ok_or_else(|| anyhow!("Iterations not set"))?;

            let salted_password = pbkdf2_sha256(self.password.as_bytes(), salt, iterations);
            let server_key = hmac_sha256(&salted_password, b"Server Key");
            let expected_sig = hmac_sha256(&server_key, auth_message.as_bytes());

            let server_sig = BASE64.decode(server_sig_b64)?;
            if server_sig != expected_sig {
                return Err(anyhow!("Server signature verification failed"));
            }
        } else {
            return Err(anyhow!("Missing server signature"));
        }

        Ok(())
    }
}

fn generate_nonce() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..18).map(|_| rng.gen()).collect();
    BASE64.encode(bytes)
}

fn saslprep(s: &str) -> String {
    // Simplified saslprep - just escape special characters
    s.replace('=', "=3D").replace(',', "=2C")
}

fn parse_scram_message(message: &str) -> Result<HashMap<String, String>> {
    let mut params = HashMap::new();
    for part in message.split(',') {
        if let Some(eq_pos) = part.find('=') {
            let key = &part[..eq_pos];
            let value = &part[eq_pos + 1..];
            params.insert(key.to_string(), value.to_string());
        }
    }
    Ok(params)
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> Vec<u8> {
    let mut result = vec![0u8; 32];
    pbkdf2::pbkdf2::<hmac::Hmac<sha2::Sha256>>(password, salt, iterations, &mut result).unwrap();
    result
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;

    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

fn xor_bytes(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect()
}
