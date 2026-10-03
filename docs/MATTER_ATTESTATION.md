# Matter device attestation, vendor ID and setup code

What a Matter controller (Apple Home, Google Home, Alexa, Home Assistant, …) checks
while it adds this node, and how to configure each part. Everything here lives in the
`[matter]` section of `config/iiotedge_default.toml`.

## Why Apple Home says "not certified"

While a node is being added the controller asks it for three things and compares them
with what the node reports about itself:

| Item | What it is | Who makes it |
|---|---|---|
| **DAC** (Device Attestation Certificate) | the node's own certificate, with its own private key | one per device, issued in the factory |
| **PAI** (Product Attestation Intermediate) | the certificate that issued the DAC | one per product line |
| **CD** (Certification Declaration) | "vendor V's products P are certified", signed by the CSA | issued after Matter certification |

The vendor ID and product ID inside the DAC, the PAI and the CD must all equal the
`VendorID`/`ProductID` the node reports. When the chain ends at a root the controller
trusts (the CSA's list of approved roots), the accessory is *certified*. Otherwise Apple
Home shows "This accessory has not been certified to work with HomeKit so some features
may not be available", and Google/Alexa want developer settings.

The firmware ships with rs-matter's **public test set** (vendor `0xFFF1`, product
`0x8001`). Every controller accepts it for development, and none can call it certified.
The notice goes away only with a real set: a vendor ID from the CSA, DACs that chain to
an approved root, and Matter certification. That is a business step; this firmware only
has to be able to *use* the result, which is what `attestation = "files"` is for.

## Configuration

```toml
[matter]
enabled = true

attestation = "files"            # "test" (default) or "files"
vendor_id  = 0x1234              # the ids the certificates were issued for
product_id = 0x00A1
dac_file     = "config/attestation/dac.der"   # DER
dac_key_file = "config/attestation/dac.key"   # PEM or DER; SEC1 or PKCS#8; or the raw 32 bytes
pai_file     = "config/attestation/pai.der"   # DER
cd_file      = "config/attestation/cd.der"    # DER (a CMS signed by the CSA)

setup_passcode = 31415926        # what a person types or scans; 1..=99999998
discriminator  = 2020            # 0..=4095
```

* `vendor_id` / `product_id` must equal the ones in the DAC. With `attestation = "test"`
  they can only be `0xFFF1` / `0x8001`: a controller compares them with the test DAC's
  and refuses anything else.
* `setup_passcode` / `discriminator` are independent of the certificates. The defaults
  (`20202021` / `3840`) are **public**: anyone on the LAN who knows them can add the
  node while its pairing window is open. Give each device its own before it leaves the
  bench. The QR code, the manual pairing code, the boot log, `--matter-qr` and
  `/onboarding/matter-qr.png` all follow these values. The passcode may not be `0`,
  `11111111` … `99999999`, `12345678` or `87654321`.
* Keep `dac_key_file` readable by the service user only (`chmod 600`); the firmware
  warns otherwise. Every device has its own DAC **and** key, so these four files are
  per-device provisioning, never part of a shared image.

## Check it before restarting the service

```
iiotedge-firmware --check-config config/iiotedge_default.toml
OK config/iiotedge_default.toml: matter=true ... attestation=files vid=0x1234 pid=0x00A1
```

With `[matter].enabled = true` the files are read and verified with the same code the
service runs at boot. A wrong set is named and explained instead of surfacing later as a
controller's "unable to add":

| Message (shortened) | Meaning |
|---|---|
| `the private key '…' does not belong to the DAC '…'` | the key's public half is not the DAC's: another device's key |
| `the DAC '…' is for vendor 0x… product 0x…, but matter.vendor_id is …` | the ids in the certificate and in the config differ |
| `the DAC '…' was not issued by the PAI '…'` | the PAI is not the DAC's issuer |
| `carries a signature that the PAI '…' did not make` | the DAC was altered or does not belong to this PAI |
| `the Certification Declaration '…' certifies vendor 0x… products …` | the CD does not cover this vendor/product id |
| `is an encrypted key; decrypt it first` | the service has no passphrase to give |
| `… is N bytes; the Matter spec allows at most 600` | a DAC/PAI over 600 bytes or a CD over 541 cannot be sent to a controller |

What is **not** checked offline: that the CD is really signed by the CSA, and that the
PAI's root is one a controller trusts. Those are what certification provides.

If the files are wrong the Matter node does **not** start (the rest of the firmware
does): presenting a different identity than the one configured would be worse than
presenting none.

## Trying the mechanism without real certificates

```
iiotedge-firmware --matter-test-attestation /tmp/att
```

writes the public test set as `dac.der`, `dac.key`, `pai.der`, `cd.der` and prints the
config lines that use them. They prove nothing to a controller (every build has them),
and Apple Home still marks the accessory uncertified, but they show the file formats and
exercise the same loading and verification a real set goes through.
`tests/matter-controller/attestation-check.sh` does exactly this end to end with a real
Matter controller (matter.js): files loaded from disk, a custom setup code, the default
code refused, plus the `--check-config` accept/refuse cases.

For lab work with your own chain, the Matter SDK's `chip-cert` can generate a development
PAA/PAI/DAC and CD. Such a chain is accepted only by controllers you configure to trust
that PAA (for example `chip-tool` with a PAA trust store); phone ecosystems accept only
the CSA's list.
