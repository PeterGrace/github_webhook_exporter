# Security review clarifications

Date: 2026-08-04 08:57:00 -0400

## Changed

- Replaced literal AES-GCM nonce and authentication-tag sizes with named constants in production code.
- Documented that post-decryption UTF-8 and repository-secret length checks defend against authenticated ciphertext produced outside this implementation.
