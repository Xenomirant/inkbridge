# Security policy

Inkbridge injects input into your computer, so a flaw in pairing or authentication is equivalent to remote control. We treat such reports as critical.

## Design guarantees
- Nothing connects without pairing. Pairing happens via a QR code shown on the host, which carries the host's certificate fingerprint and a one-time 128-bit secret.
- All traffic is encrypted and mutually authenticated (QUIC/TLS 1.3 with pinned certificates).
- Every incoming pen sample is sanitised (`PenSample::sanitized`) before it reaches the OS, and message sizes are bounded (`MAX_MESSAGE_LEN`).

## Reporting a vulnerability
Please **don't open a public issue**. Use GitHub's private vulnerability reporting ("Security" tab → "Report a vulnerability"). We aim to acknowledge reports within 72 hours.
