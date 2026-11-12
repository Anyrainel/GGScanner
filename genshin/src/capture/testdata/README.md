# Synthetic login fixture

`token7_2.bin` is a fully framed, synthetic Genshin 7.2 login response. It contains
fictional game UID `800000001`, distinct fictional HoYoverse account identifier
`900000002`, key ID 5 and seed `123456789` encrypted with the public counterpart
of the bundled key. Header timestamp is `1700000000000`. No live login traffic,
authentication token or private account data is included.

It exercises production framing, incoming token verification and UID extraction
in the capture monitor tests. Tests change the fictional UID for account switches;
the encrypted seed and all other data remain synthetic.
