# Zeroizing decryption transfer

Date: 2026-08-04 09:31:21 -0400

## Changed

- Store repository secrets as zeroizing secret byte slices with validated UTF-8 exposure.
- Transfer authenticated plaintext directly from the decryption buffer into secret storage without creating a second plaintext allocation.
- Add a regression test proving the decrypted allocation is transferred rather than copied.
