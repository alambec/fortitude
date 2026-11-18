# Security

Fortitude stores medical images, which contain health data. If you find a vulnerability, report it
privately: do not open a public issue.

- Use GitHub's private vulnerability reporting (the "Report a vulnerability" button of the
  Security tab), or write to gabriel.couture@alambec.com.
- Include the version or commit, the configuration involved, and the steps to reproduce.

## Deployment

- Fortitude serves plain HTTP: put it behind a reverse proxy that terminates TLS.
- Without any `auth.users` or `auth.tokens`, authentication is disabled: configure them before
  exposing the HTTP port beyond a trusted network.
- DIMSE has no authentication beyond AE titles: restrict its port at the network level.
