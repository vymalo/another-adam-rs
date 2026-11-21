# Test fixtures: a CA and a server certificate

**Test only.** These files sign and serve nothing but the `MODEL_CA_FILE` tests (`tests/ca_file.rs` here and
`crates/adam-service/tests/model_auth.rs`). The private key is not a secret: never trust this CA anywhere else.

| File | What |
|---|---|
| `test-ca.pem` | a self-signed CA (P-256), valid until 2126 |
| `test-server.pem` | a server certificate for `localhost` and `127.0.0.1`, signed by the CA |
| `test-server.key` | the server certificate's private key (PKCS#8) |

To make them again (the CA's key is thrown away):

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out test-ca.pem \
  -days 36500 -subj "/CN=adam-model-openai test CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout test-server.key -out server.csr \
  -subj "/CN=localhost"
printf 'subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > ext.cnf
openssl x509 -req -in server.csr -CA test-ca.pem -CAkey ca.key -CAcreateserial -out test-server.pem \
  -days 36500 -extfile ext.cnf
rm ca.key server.csr ext.cnf test-ca.srl
```
