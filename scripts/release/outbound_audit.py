"""Linux OS-level outbound-attempt audit for a command and all its descendants.

Counts attempted connect/sendto/sendmsg/sendmmsg destinations, including failed
syscalls, from exec through final child exit. Unix/kernel IPC and loopback are
allowed; DNS port 53 is denied even on loopback (a local resolver may forward it).
Raw traces use -s 0 and are deleted; reports contain addresses/counts, no payloads.
Exit 86 means detected outbound attempts; 87 means incomplete/unparseable evidence.
"""
import argparse
import ipaddress
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

DENIED = 86
INCOMPLETE = 87
CALL = re.compile(r'^(connect|sendto|sendmsg|sendmmsg)\(')
ADDR = re.compile(r'sa_family=AF_INET6?\b[^{}]*')


def analyze(paths):
    events, unknown = [], 0
    for path in sorted(paths):
        pending = None
        for line in path.read_text().splitlines():
            if '<unfinished ...>' in line:
                if pending is not None:
                    unknown += 1
                pending = line.replace('<unfinished ...>', '')
                continue
            if line.startswith('<... ') and ' resumed>' in line:
                if pending is None:
                    unknown += 1
                    continue
                line = pending + line.split(' resumed>', 1)[1]
                pending = None
            if not CALL.match(line):
                if line and not line.startswith(('+++', '---')):
                    unknown += 1
                continue
            call = CALL.match(line).group(1)
            destinations = ADDR.findall(line)
            if not destinations:
                # NULL means send on a connected socket (its connect was traced).
                if 'sa_family=AF_' in line and not any(
                    family in line for family in ('AF_UNIX', 'AF_NETLINK', 'AF_UNSPEC')
                ):
                    unknown += 1
                continue
            for destination in destinations:
                match = re.search(r'(?:inet_addr\(|inet_pton\(AF_INET6, )"([^"]+)"', destination)
                port = re.search(r'sin6?_port=htons\((\d+)\)', destination)
                if match is None or port is None:
                    unknown += 1
                    continue
                try:
                    address = ipaddress.ip_address(match.group(1))
                except ValueError:
                    unknown += 1
                    continue
                effective = address.ipv4_mapped if isinstance(address, ipaddress.IPv6Address) else None
                local = (effective or address).is_loopback and int(port.group(1)) != 53
                events.append({'syscall': call, 'address': str(address), 'port': int(port.group(1)),
                               'external': not local, 'failed': bool(re.search(r'= -1\b', line))})
        if pending is not None:
            unknown += 1
    external = [event for event in events if event['external']]
    return {'schema': 'aelix-studio-host.outbound-audit/1', 'traceFiles': len(paths),
            'destinationAttempts': len(events), 'externalAttempts': len(external),
            'unparsedRecords': unknown, 'external': external}


def audit(command, report_path):
    if sys.platform != 'linux' or shutil.which('strace') is None:
        raise RuntimeError('Linux strace is required; audit cannot be skipped')
    with tempfile.TemporaryDirectory(prefix='host-outbound-audit-') as directory:
        prefix = Path(directory) / 'network'
        # -ff follows forks/threads and waits for all descendants. -s 0 suppresses
        # buffer contents; do not add execve/read/write tracing (secrets/payloads).
        process = subprocess.run(['strace', '-ff', '-qq', '-s', '0', '-e',
                                  'trace=connect,sendto,sendmsg,sendmmsg', '-o', str(prefix), '--', *command])
        paths = list(prefix.parent.glob(prefix.name + '.*'))
        report = analyze(paths)
    report['commandExitCode'] = process.returncode
    complete = bool(report['traceFiles']) and report['unparsedRecords'] == 0
    code = INCOMPLETE if not complete else DENIED if report['externalAttempts'] else process.returncode
    report['status'] = 'passed' if code == 0 else 'failed'
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps({key: report[key] for key in ('status', 'externalAttempts', 'unparsedRecords', 'commandExitCode')}))
    return code


def positive_control():
    """Deliberately attempt denied TCP and UDP, including on a network-none Docker."""
    import socket
    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        with socket.socket(socket.AF_INET, kind) as connection:
            connection.settimeout(0.2)
            try:
                if kind == socket.SOCK_STREAM:
                    connection.connect(('203.0.113.1', 443))
                else:
                    connection.sendto(b'control', ('203.0.113.1', 53))
            except OSError:
                pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    parser.add_argument('--positive-control', action='store_true')
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.positive_control:
        positive_control()
        return 0
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or not args.report:
        parser.error('--report and command after -- are required')
    return audit(command, args.report)


if __name__ == '__main__':
    sys.exit(main())
