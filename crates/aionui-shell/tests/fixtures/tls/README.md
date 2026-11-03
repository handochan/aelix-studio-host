Public test-only certificate and private key for loopback TLS regression tests.
The certificate names `localhost` (DNS only), is self-signed, and is valid from
2026-11-03 for 100 years. It is never a production credential or trust anchor.

Generate a fresh pair with OpenSSL:

```sh
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out key.pem
openssl req -new -x509 -config openssl.cnf -key key.pem -days 36500 -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost' \
  -addext 'basicConstraints=critical,CA:FALSE' \
  -addext 'keyUsage=critical,digitalSignature' \
  -addext 'extendedKeyUsage=serverAuth' -out cert.pem
openssl x509 -in cert.pem -outform DER -out localhost.der
openssl pkcs8 -topk8 -nocrypt -in key.pem -outform DER -out localhost-key.der
rm key.pem cert.pem
```
