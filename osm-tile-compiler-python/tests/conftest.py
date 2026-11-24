"""Shared fixtures: Monaco, compiled once without the supplemental datasets."""

from pathlib import Path

import pytest

import osm_tile_compiler as otc

MONACO = Path(__file__).resolve().parents[2] / "fixtures" / "monaco.osm.pbf"


def compile_monaco(directory: Path, file_name: str = "monaco.pmtiles", **options) -> dict:
    options.setdefault("omit_sources", ["all"])
    options.setdefault("threads", 2)
    return otc.compile(
        MONACO,
        directory / file_name,
        directory / "sources",
        directory / "temp",
        **options,
    )


@pytest.fixture(scope="session")
def monaco(tmp_path_factory) -> Path:
    """A Monaco basemap, zooms 0 to 14. Tests must not change it."""
    directory = tmp_path_factory.mktemp("monaco")
    return Path(compile_monaco(directory)["archive"])
