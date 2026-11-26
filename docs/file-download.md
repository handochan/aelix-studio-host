# Preview original downloads

Studio checks `GET /api/system/info` on the running Host. When
`data.file_content_binary_post` is `true`, Studio sends `POST /api/fs/content`
with `Content-Type: application/json`,
`Accept: application/octet-stream` and the following JSON body.

```json
{
  "file": { "kind": "project", "pe_id": "project-id", "relative_path": "docs/report.pdf" }
}
```

The body accepts the existing project, local and upload variants of
`ChatFileRef`. Use the same authentication and CSRF rules as other content
reads. The Host resolves the reference with read permission and streams the
original bytes with `Content-Type: application/octet-stream` on success.
The URL carries no file reference.

When the capability is absent, Studio uses the existing raw GET for every
file size. Preview download errors on that path can include the reference
in Chromium's URL log. The default installation gains the path-free
request after Studio pins a Host release that advertises the capability.
Existing callers without the explicit binary Accept header continue to
receive JSON.

Keep the legacy `GET /api/fs/stream` query form available for at least one
release. It still supports byte ranges for existing clients, including
PDF embeds.
