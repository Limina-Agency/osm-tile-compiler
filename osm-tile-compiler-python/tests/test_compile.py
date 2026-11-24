from pathlib import Path

import osm_tile_compiler as otc
import pytest

from conftest import MONACO, compile_monaco


def test_compile_reports_its_output_and_progress(tmp_path):
    reports = []
    result = compile_monaco(tmp_path, max_zoom=12, progress=reports.append)

    archive = tmp_path / "monaco.pmtiles"
    assert result["archive"] == str(archive)
    assert archive.is_file()
    assert result["format"] == "pmtiles"
    assert result["building_levels"] is None
    assert sorted(result["omitted_sources"]) == sorted(otc.SUPPLEMENTAL_DATASETS)
    assert result["result"]["tiles"] > 0
    assert result["manifest"]["format"] == "basemap-manifest"
    west, south, east, north = result["bounds"]
    assert 7.3 < west < east < 7.5 and 43.7 < south < north < 43.8

    assert reports, "progress was never called"
    assert all(isinstance(report, otc.Progress) for report in reports)
    assert {report.stage for report in reports} <= set(otc.COMPILE_STAGES)
    overall = [report.overall for report in reports]
    assert overall == sorted(overall)
    assert all(0.0 <= value <= 1.0 for value in overall)
    assert "Progress(stage=" in repr(reports[0])

    assert otc.inspect(archive)["zooms"] == [0, 12]


def test_compile_options_reach_the_archive(tmp_path):
    result = compile_monaco(
        tmp_path,
        "monaco.mbtiles",
        min_zoom=2,
        max_zoom=10,
        layers=["water", "transportation"],
        name="Monaco test",
        languages=["en"],
    )
    assert result["format"] == "mbtiles"
    assert (tmp_path / "monaco.mbtiles").is_file()
    assert result["manifest"]["result"]["tiles"] == result["result"]["tiles"]


def test_compile_writes_building_levels_beside_the_archive(tmp_path):
    result = compile_monaco(tmp_path, building_levels=True)
    # Buildings are drawn from zoom 13, so the levels need a z13+ compile.
    levels = result["building_levels"]
    assert levels is not None
    assert Path(levels).is_file()


@pytest.mark.parametrize(
    "options, message",
    [
        ({"layers": ["roads"]}, "roads"),
        ({"layers": ["water"], "exclude_layers": ["poi"]}, "not both"),
        ({"min_zoom": 9, "max_zoom": 3}, "zooms must satisfy"),
        ({"threads": 0}, "threads must be positive"),
        ({"sources_policy": "always"}, "sources_policy"),
        ({"omit_sources": ["coastlines"]}, "coastlines"),
        ({"read": "near_area"}, "bounds or a polygon"),
        ({"format": "geojson"}, "geojson"),
        ({"languages": ["xx"]}, "xx"),
    ],
)
def test_bad_requests_raise_invalid_request(tmp_path, options, message):
    with pytest.raises(otc.InvalidRequest, match=message) as raised:
        compile_monaco(tmp_path, **options)
    assert raised.value.kind == "invalid_request"
    assert raised.value.stage is None
    assert isinstance(raised.value, otc.Error)
    assert not (tmp_path / "monaco.pmtiles").exists()


def test_missing_sources_are_refused_unless_omitted(tmp_path):
    with pytest.raises(otc.InvalidRequest):
        compile_monaco(tmp_path, omit_sources=[])


def test_an_existing_output_is_not_replaced(tmp_path):
    (tmp_path / "monaco.pmtiles").write_bytes(b"keep")
    with pytest.raises(otc.InvalidRequest):
        compile_monaco(tmp_path)
    assert (tmp_path / "monaco.pmtiles").read_bytes() == b"keep"


def test_a_missing_input_raises(tmp_path):
    with pytest.raises(otc.Error):
        otc.compile(
            tmp_path / "nowhere.osm.pbf",
            tmp_path / "out.pmtiles",
            tmp_path / "sources",
            tmp_path / "temp",
            omit_sources=["all"],
        )


def test_paths_may_be_strings(tmp_path):
    result = otc.compile(
        str(MONACO),
        str(tmp_path / "monaco.pmtiles"),
        str(tmp_path / "sources"),
        str(tmp_path / "temp"),
        omit_sources=["all"],
        max_zoom=6,
    )
    assert result["result"]["tiles"] > 0
