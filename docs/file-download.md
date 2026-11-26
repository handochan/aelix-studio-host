# Preview original downloads

Studio sends `POST /api/fs/content` with `Content-Type: application/json`,
`Accept: application/octet-stream` and the following JSON body.

```json
{
  "file": { "kind": "project", "pe_id": "project-id", "relative_path": "docs/report.pdf" },
  "encoding": "base64"
}
```

The body accepts the existing project, local and upload variants of
`ChatFileRef`. Use the same authentication and CSRF rules as other content
reads. The Host resolves the reference with read permission and streams the
original bytes with `Content-Type: application/octet-stream` on success.
The URL carries no file reference.

Older Hosts return their existing JSON envelope with the base64 string in
`data`. Studio recognizes the response format by its content type and
decodes the JSON fallback in memory. The older Host's 256 MiB content read
limit applies to that fallback. Existing callers without the explicit
binary Accept header continue to receive JSON.

Keep the legacy `GET /api/fs/stream` query form available for at least one
release. It still supports byte ranges for existing clients, including
PDF embeds. Studio original downloads use the body-based request on both
Host versions.
