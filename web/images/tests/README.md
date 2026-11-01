# Image page tests

`vectors.json` holds AWS's Signature Version 4 test vectors, taken unchanged
from the `tests/aws-signing-test-suite/v4` directory of
[awslabs/aws-c-auth](https://github.com/awslabs/aws-c-auth) at commit
`3d29adcecf5c032131acee9f4125a636d2aaea6a` (Apache License 2.0). Each entry
keeps the original `context.json` as `context` and the verbatim text of
`request.txt`, `header-canonical-request.txt`, `header-signature.txt`,
`query-canonical-request.txt` and `query-signature.txt` under the matching camel
case names, with only the signature files trimmed. The `*-normalized` cases are
omitted because S3 does not normalize paths; their `*-unnormalized` twins are the
S3 behaviour and are kept. The credentials inside are the public example values
from the AWS documentation.

`listing.xml` is a ListObjectsV2 response with hostile keys for the parser test.
`stack.mjs` starts a throwaway Garage and Caddy for `site.test.mjs`; see
[docs/images.md](../../../docs/images.md#tests).
