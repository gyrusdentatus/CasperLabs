from pathlib import Path

from casperlabs_client import CasperLabsClient
from casperlabs_client.arg_types import directory_for_write
from casperlabs_client.decorators import guarded_command

NAME: str = "keygen"
HELP: str = """Generates account keys into existing directory

Usage: casperlabs-client keygen <existing output directory>
Command will override existing files!
Generated files:
   account-hash         # Hash of public key for use on the system as file
   account-hash-hex     # Hash of public key for use on the system as hex text
   account-private.pem  # ed25519 private key
   account-public.pem   # ed25519 public key"""
OPTIONS = [
    [
        ("directory",),
        dict(
            type=directory_for_write,
            help="Output directory for keys. Should already exists.",
        ),
    ]
]


@guarded_command
def method(casperlabs_client: CasperLabsClient, args):
    directory = Path(args.get("directory")).resolve()
    casperlabs_client.keygen(directory)
    print(f"Keys successfully created in directory: {str(directory.absolute())}")