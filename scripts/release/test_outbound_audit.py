"""Parser and real syscall positive controls; no provider credentials required."""
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import outbound_audit as audit


class OutboundAuditTest(unittest.TestCase):
    def parse(self, lines):
        with tempfile.TemporaryDirectory() as root:
            trace = Path(root) / 'network.1'
            trace.write_text(lines)
            return audit.analyze([trace])

    def test_failed_tcp_and_udp_are_counted_ipv4_ipv6(self):
        result = self.parse('''connect(3, {sa_family=AF_INET, sin_port=htons(443), sin_addr=inet_addr("203.0.113.1")}, 16) = -1 ENETUNREACH
sendto(3, ""..., 7, 0, {sa_family=AF_INET6, sin6_port=htons(53), inet_pton(AF_INET6, "2001:db8::1", &sin6_addr)}, 28) = -1 ENETUNREACH
''')
        self.assertEqual(result['externalAttempts'], 2)
        self.assertTrue(all(event['failed'] for event in result['external']))
        self.assertEqual(result['unparsedRecords'], 0)

    def test_loopback_mapped_and_ipc_allowed_but_dns_denied(self):
        result = self.parse('''connect(3, {sa_family=AF_UNIX, sun_path="/tmp/socket"}, 16) = 0
connect(3, {sa_family=AF_INET6, sin6_port=htons(80), inet_pton(AF_INET6, "::ffff:127.0.0.1", &sin6_addr)}, 28) = 0
connect(3, {sa_family=AF_INET, sin_port=htons(53), sin_addr=inet_addr("127.0.0.53")}, 16) = 0
''')
        self.assertEqual(result['externalAttempts'], 1)
        self.assertEqual(result['unparsedRecords'], 0)

    def test_unfinished_and_sendmsg_destinations(self):
        result = self.parse('''connect(3, {sa_family=AF_INET, sin_port=htons(80), sin_addr=inet_addr("127.0.0.1")}, 16 <unfinished ...>
<... connect resumed>) = 0
sendmsg(4, {msg_name={sa_family=AF_INET, sin_port=htons(53), sin_addr=inet_addr("203.0.113.1")}, msg_namelen=16}, 0) = 7
''')
        self.assertEqual(result['externalAttempts'], 1)
        self.assertEqual(result['unparsedRecords'], 0)

    def test_unknown_and_incomplete_are_not_zero_proof(self):
        result = self.parse('connect(3, {sa_family=AF_INET, broken}, 16) = -1 EINVAL\nconnect(3, <unfinished ...>\n')
        self.assertEqual(result['unparsedRecords'], 2)

    @unittest.skipUnless(sys.platform == 'linux' and shutil.which('strace'), 'Linux strace required')
    def test_real_positive_control_fails_audit(self):
        with tempfile.TemporaryDirectory() as root:
            report = Path(root) / 'report.json'
            result = subprocess.run([sys.executable, '-I', '-B', str(Path(audit.__file__)),
                                     '--report', str(report), '--', sys.executable, '-I', '-B',
                                     str(Path(audit.__file__)), '--positive-control'], capture_output=True, text=True)
            self.assertEqual(result.returncode, audit.DENIED, result.stdout + result.stderr)
            evidence = json.loads(report.read_text())
            self.assertGreaterEqual(evidence['externalAttempts'], 2)
            self.assertEqual(evidence['unparsedRecords'], 0)
            self.assertEqual({item['syscall'] for item in evidence['external']}, {'connect', 'sendto'})


if __name__ == '__main__':
    unittest.main()
