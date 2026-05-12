# SPDX-License-Identifier: MIT OR Apache-2.0

#!/usr/bin/python3

"""
    A tool that takes DNS seeds dump data and formats it for floresta's json

    Usage:
        make_seed.py input_file.txt output.json
"""

import argparse
import ipaddress
import json


def is_special_address(addr):
    addr = addr.lower()
    return ".onion" in addr or ".i2p" in addr


def parse_line(line):
    parts = line.split()

    # Skip invalid or header lines
    if len(parts) < 11 or parts[0].startswith("#"):
        return None

    address = parts[0]

    # Skip onion/i2p
    if is_special_address(address):
        return None

    is_good = parts[1]
    if is_good != "1":
        return None

    last_seen = int(parts[2])
    services_hex = parts[9]

    # Split IP and port
    if address.startswith('['):  # IPv6 like [::1]:8333
        ip, port = address.rsplit(']:', 1)
        ip = ip.strip('[]')
    else:
        ip, port = address.rsplit(':', 1)

    port = int(port)

    # Detect IP version safely
    try:
        ip_obj = ipaddress.ip_address(ip)
    except ValueError:
        return None  # skip anything that isn't a real IP

    if ip_obj.is_private:
        return None

    ip_type = "V6" if ip_obj.version == 6 else "V4"

    # Convert services hex -> base10
    services = int(services_hex, 16)

    return {
        "address": {
            ip_type: ip
        },
        "last_connected": last_seen,
        "state": {
            "Tried": last_seen,
        },
        "services": services,
        "port": port
    }


def convert_file(input_file, output_file):
    results = []

    with open(input_file, "r") as f:
        for line in f:
            parsed = parse_line(line)
            if parsed:
                results.append(parsed)

    with open(output_file, "w") as f:
        json.dump(results, f, indent=4)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Convert IP list to JSON")
    parser.add_argument("input", help="Input file path")
    parser.add_argument("output", help="Output JSON file path")

    args = parser.parse_args()

    convert_file(args.input, args.output)
