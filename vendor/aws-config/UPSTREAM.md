# aws-config source and Host changes

The source comes from the published `aws-config` 1.13.0 crate. Its archive SHA-256 is
`939e256cfe0ed68e96d32f194339d88210d5c10685cff0787400158f852c6819`.
The package identifies `sdk/aws-config` at AWS SDK for Rust commit
`b964fc77de60b0a492ff788c40bb66524fd26a44`. The published license and
Cargo provenance metadata are retained.

The local manifest adds the Host environment policy. Before starting an
external `credential_process`, the provider applies that policy to the
existing platform command. Profile selection, credentials parsing, command
quoting and the rest of the SDK provider chain use the published implementation.
