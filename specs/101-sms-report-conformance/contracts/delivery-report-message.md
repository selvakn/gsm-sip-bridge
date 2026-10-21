# Contract: delivery-report MESSAGE

Example after the change, for a line with a Gm SA, one `Service-Route`, and the setting on. All numbers and hosts are synthetic.

```text
MESSAGE sip:ip-sm-gw@mmtel.ims.example.org SIP/2.0
Via: SIP/2.0/TCP 192.0.2.10:35044;branch=z9hG4bK610f2b9f1b5a;rport
Max-Forwards: 70
Route: <sip:198.51.100.7:9900;lr>
Route: <sip:orig@scscf.ims.example.org;lr>
Require: sec-agree
Proxy-Require: sec-agree
Security-Verify: ipsec-3gpp;alg=hmac-sha-1-96;prot=esp;mod=trans;ealg=null;spi-c=1;spi-s=2;port-c=9950;port-s=9900
P-Preferred-Identity: <sip:+919000000000@ims.example.org>
From: <sip:+919000000000@ims.example.org>;tag=2f98c410
To: <sip:ip-sm-gw@mmtel.ims.example.org>
Call-ID: 31c04dde5f3ebad4
In-Reply-To: asbc5lyog6o3coyscqscs36drywqw3qswgy1@tas.ims.example.org.23
CSeq: 4 MESSAGE
Content-Type: application/vnd.3gpp.sms
Content-Length: 6

<02 01 41 02 00 00>
```

Rules:

1. `In-Reply-To` equals the inbound `Call-ID` byte for byte. Omitted (with a warning) only if the inbound request had none.
2. The first `Route` is the P-CSCF with `lr`: the protected server port when a Gm SA exists, otherwise the address registered through. IPv6 is bracketed.
3. The P-CSCF `Route` precedes every `Service-Route` entry, which keep their order. With no `Service-Route`, it is the only `Route`.
4. With `sms_report_pcscf_route = false`, rule 2 and 3's P-CSCF line is absent and the route set is exactly today's.
5. No other header or body byte differs from the pre-change report.
