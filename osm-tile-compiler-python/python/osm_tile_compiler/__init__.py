"""Compile OpenStreetMap extracts into OpenMapTiles basemaps, and cut, diff,
patch and check the archives.

Python bindings for the osm-tile-compiler Rust library. Every function
releases the GIL while it works. ``compile`` and ``update`` take a
``progress`` callable, called with a :class:`Progress` on the calling thread;
``compile``, ``extract`` and ``update`` take ``cancel``, a
:class:`CancelToken` or a callable returning True to stop, and stop on Ctrl-C
too. A stopped call raises :class:`Cancelled` (or the KeyboardInterrupt) and
leaves nothing at its output.

Output derived from OpenStreetMap data is under the ODbL: credit
"© OpenStreetMap contributors".
"""

from ._native import (
    COMPILE_STAGES,
    LANGUAGES,
    LAYERS,
    OMT_SCHEMA_VERSION,
    SUPPLEMENTAL_DATASETS,
    ArchiveInvalid,
    CancelToken,
    Cancelled,
    DownloadError,
    Error,
    Failed,
    InvalidRequest,
    OutOfSpace,
    Progress,
    __version__,
    apply,
    compile,
    diff,
    extract,
    inspect,
    read_delta_manifest,
    update,
    verify,
)

__all__ = [
    "COMPILE_STAGES",
    "LANGUAGES",
    "LAYERS",
    "OMT_SCHEMA_VERSION",
    "SUPPLEMENTAL_DATASETS",
    "ArchiveInvalid",
    "CancelToken",
    "Cancelled",
    "DownloadError",
    "Error",
    "Failed",
    "InvalidRequest",
    "OutOfSpace",
    "Progress",
    "__version__",
    "apply",
    "compile",
    "diff",
    "extract",
    "inspect",
    "read_delta_manifest",
    "update",
    "verify",
]
