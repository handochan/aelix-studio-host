`huge-symlink.zip` is a synthetic 112-byte ZIP64 archive for RUSTSEC-2026-0429
regression coverage. It contains one Stored entry named `l`, an empty payload,
Unix mode `0120777`, and central-directory uncompressed size `2^62`. Its local
header is 31 bytes, central record is 59 bytes (including the 12-byte ZIP64
extra field), and end record is 22 bytes. It contains no executable or user data.

The import test verifies the size and symlink metadata before checking rejection.
