#!/usr/bin/env python3
"""WS-Discovery probe helper for the functional test harness.

Sends an ONVIF WS-Discovery Probe to the multicast group and exits 0 if the
firmware answers with a ProbeMatch pointing at its device service. Used by
tests/verify.sh; standalone so the shell script stays readable.
"""
import socket
import sys
import uuid

PROBE = f"""<?xml version="1.0" encoding="UTF-8"?>
<e:Envelope xmlns:e="http://www.w3.org/2003/05/soap-envelope"
 xmlns:w="http://schemas.xmlsoap.org/ws/2004/08/addressing"
 xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery"
 xmlns:dn="http://www.onvif.org/ver10/network/wsdl">
<e:Header>
<w:MessageID>uuid:{uuid.uuid4()}</w:MessageID>
<w:To>urn:schemas-xmlsoap-org:ws:2005:04:discovery</w:To>
<w:Action>http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</w:Action>
</e:Header>
<e:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></e:Body>
</e:Envelope>"""


def main() -> int:
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(4)
    # Send to the WS-Discovery multicast group; the firmware also loops back
    # on the local host so this works on a single machine.
    try:
        sock.sendto(PROBE.encode(), ("239.255.255.250", 3702))
        deadline = 4
        sock.settimeout(deadline)
        while True:
            data, _ = sock.recvfrom(65535)
            text = data.decode(errors="replace")
            if "ProbeMatch" in text and "device_service" in text:
                return 0
    except socket.timeout:
        return 1
    except OSError as exc:
        print(f"ws_probe: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
