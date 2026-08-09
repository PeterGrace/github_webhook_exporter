# Issue 46 blocking push fixes

- Restricted external Secret reference allowances to the `name`, `key`, and `optional` metadata
  fields, while continuing to scan credential-shaped descendants.
- Canonicalized archive extraction targets before duplicate detection, including dot-component and
  file/directory trailing-slash aliases.
- Replaced the output commit gap with identity-bound Linux `renameat2` exchange/no-replace
  operations, parent locking, commit-time revalidation, and no-follow cleanup of only the known
  generated directory.
- Added focused atomic replacement and destination/parent substitution regressions.
- Required every security self-test negative probe to emit its exact stable diagnostic, including a
  mutation fixture proving unrelated failures are rejected.
