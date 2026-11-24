# Type stubs for the extension module; the docstrings are in __init__.py and
# the README.
from collections.abc import Callable, Sequence
from os import PathLike
from typing import Any, Literal

_Path = str | PathLike[str]
#: (west, south, east, north), degrees.
_Bounds = tuple[float, float, float, float]
_Cancel = CancelToken | Callable[[], bool] | None
_Report = dict[str, Any]

__version__: str
OMT_SCHEMA_VERSION: str
LAYERS: list[str]
LANGUAGES: list[str]
SUPPLEMENTAL_DATASETS: list[str]
#: Compile stage ids, in order, with their typical weight in `overall`.
COMPILE_STAGES: dict[str, float]

class Error(Exception):
    kind: str
    stage: str | None

class Cancelled(Error): ...
class InvalidRequest(Error): ...
class DownloadError(Error): ...
class OutOfSpace(Error): ...
class Failed(Error): ...

class ArchiveInvalid(Error):
    report: _Report

class CancelToken:
    def __init__(self) -> None: ...
    def cancel(self) -> None: ...
    @property
    def cancelled(self) -> bool: ...

class Progress:
    @property
    def stage(self) -> str: ...
    @property
    def message(self) -> str: ...
    @property
    def fraction(self) -> float | None: ...
    @property
    def overall(self) -> float | None: ...

def compile(
    input_pbf: _Path,
    output: _Path,
    sources: _Path,
    temp_dir: _Path,
    *,
    extra_inputs: Sequence[_Path] = ...,
    format: Literal["pmtiles", "mbtiles"] | None = None,
    sources_policy: Literal["use_present", "fetch_missing", "fetch_and_verify"] = "use_present",
    omit_sources: Sequence[str] = ...,
    bounds: _Bounds | None = None,
    polygon: _Path | None = None,
    read: Literal["whole_extract", "near_area"] = "whole_extract",
    center: tuple[float, float, int] | None = None,
    name: str | None = None,
    description: str | None = None,
    attribution: str | None = None,
    gzip_level: int | None = None,
    min_zoom: int | None = None,
    max_zoom: int | None = None,
    threads: int | None = None,
    layers: Sequence[str] | None = None,
    exclude_layers: Sequence[str] | None = None,
    languages: Sequence[str] | None = None,
    building_levels: bool = False,
    search_index: bool = False,
    tile_format: Literal["mvt", "mlt"] = "mvt",
    sar: bool = False,
    sort_memory: int | None = None,
    schema: str | None = None,
    progress: Callable[[Progress], object] | None = None,
    cancel: _Cancel = None,
) -> _Report: ...
def extract(
    source: _Path,
    output: _Path,
    *,
    bounds: _Bounds | None = None,
    polygon: _Path | None = None,
    min_zoom: int | None = None,
    max_zoom: int | None = None,
    cancel: _Cancel = None,
) -> _Report: ...
def update(
    input: _Path,
    output: _Path,
    *,
    change_files: Sequence[_Path] = ...,
    replication: str | None = None,
    until: int | None = None,
    max_diffs: int | None = None,
    clip: Literal["header", "none"] | _Bounds | None = None,
    threads: int | None = None,
    progress: Callable[[Progress], object] | None = None,
    cancel: _Cancel = None,
) -> _Report: ...
def diff(base: _Path, target: _Path, output: _Path) -> _Report: ...
def apply(base: _Path, delta: _Path, output: _Path) -> _Report: ...
def read_delta_manifest(path: _Path) -> _Report: ...
def inspect(path: _Path) -> _Report: ...
def verify(path: _Path) -> _Report: ...
