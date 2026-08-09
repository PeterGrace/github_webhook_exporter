# Production image GHCR publication

- Added stable `vMAJOR.MINOR.PATCH` release validation across Cargo and Helm metadata.
- Added digest-pinned, tag-only GHCR publication for the supported `linux/amd64` image.
- Kept pull requests and `main` validation-only and scoped package write permission to publication.
- Smoke-tested the exact local release image before authentication and rejected existing tags.
- Documented immutable image coordinates, release creation, pulls, and failure recovery.
