import osm_tile_compiler as otc
import pytest

# Monaco's old town and harbour, well inside the extract.
INNER = (7.415, 43.728, 7.43, 43.74)


def test_inspect_reads_the_archive(monaco):
    report = otc.inspect(monaco)
    assert report["tile_type"] == "mvt"
    assert report["zooms"] == [0, 14]
    assert report["problem_count"] == 0
    assert report["problems"] == []
    assert report["addressed_tiles"] > 0
    assert report["metadata"]["vector_layers"]
    assert report["tile_stats"]


def test_verify_returns_the_report_of_a_valid_archive(monaco):
    assert otc.verify(monaco)["problem_count"] == 0


def test_inspect_raises_for_a_file_that_is_not_pmtiles(tmp_path):
    path = tmp_path / "not.pmtiles"
    path.write_bytes(b"hello" * 100)
    with pytest.raises(otc.Error, match="not a PMTiles archive"):
        otc.inspect(path)


def test_verify_raises_archive_invalid_with_the_report(monaco, tmp_path):
    damaged = tmp_path / "damaged.pmtiles"
    data = bytearray(monaco.read_bytes())
    report = otc.inspect(monaco)
    # Overwrite the middle of the tile data; the header and directories stay.
    middle = len(data) * 3 // 4
    data[middle : middle + 4096] = b"\xff" * 4096
    damaged.write_bytes(bytes(data))
    assert otc.inspect(damaged)["problem_count"] > 0
    with pytest.raises(otc.ArchiveInvalid) as raised:
        otc.verify(damaged)
    assert raised.value.kind == "archive_invalid"
    assert raised.value.report["problem_count"] > 0
    assert raised.value.report["file_bytes"] == report["file_bytes"]


def test_extract_cuts_an_area_and_zooms(monaco, tmp_path):
    output = tmp_path / "inner.pmtiles"
    report = otc.extract(monaco, output, bounds=INNER, min_zoom=10, max_zoom=13)
    assert report["zooms"] == [10, 13]
    assert 0 < report["tiles"] < report["source_tiles"]
    assert otc.verify(output)["addressed_tiles"] == report["tiles"]


def test_extract_with_a_polygon(monaco, tmp_path):
    west, south, east, north = INNER
    polygon = tmp_path / "area.geojson"
    polygon.write_text(
        '{"type":"Polygon","coordinates":[[[%s,%s],[%s,%s],[%s,%s],[%s,%s],[%s,%s]]]}'
        % (west, south, east, south, east, north, west, north, west, south)
    )
    report = otc.extract(monaco, tmp_path / "poly.pmtiles", polygon=polygon)
    assert 0 < report["tiles"] < report["source_tiles"]


def test_extract_refuses_an_area_outside_the_archive(monaco, tmp_path):
    with pytest.raises(otc.InvalidRequest):
        otc.extract(monaco, tmp_path / "none.pmtiles", bounds=(100.0, 10.0, 101.0, 11.0))


def test_diff_and_apply_rebuild_the_target(monaco, tmp_path):
    base = tmp_path / "base.pmtiles"
    otc.extract(monaco, base, bounds=INNER)
    delta = tmp_path / "update.delta"
    diffed = otc.diff(base, monaco, delta)
    assert diffed["delta_bytes"] == delta.stat().st_size
    assert diffed["tiles"]["added"] > 0

    manifest = otc.read_delta_manifest(delta)
    assert manifest["format"] == "osm-tile-compiler-delta"
    assert manifest["target"]["bytes"] == monaco.stat().st_size

    rebuilt = tmp_path / "rebuilt.pmtiles"
    applied = otc.apply(base, delta, rebuilt)
    assert applied["target"] == manifest["target"]
    assert rebuilt.read_bytes() == monaco.read_bytes()


def test_apply_refuses_the_wrong_base(monaco, tmp_path):
    base = tmp_path / "base.pmtiles"
    otc.extract(monaco, base, bounds=INNER)
    delta = tmp_path / "update.delta"
    otc.diff(base, monaco, delta)
    with pytest.raises(otc.Error):
        otc.apply(monaco, delta, tmp_path / "rebuilt.pmtiles")
    assert not (tmp_path / "rebuilt.pmtiles").exists()
