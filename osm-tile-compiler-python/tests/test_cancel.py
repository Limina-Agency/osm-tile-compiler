import threading

import osm_tile_compiler as otc
import pytest

from conftest import compile_monaco


def test_a_cancelled_token_stops_the_compile(tmp_path):
    token = otc.CancelToken()
    assert not token.cancelled
    token.cancel()
    assert token.cancelled
    assert repr(token) == "CancelToken(cancelled=True)"
    with pytest.raises(otc.Cancelled) as raised:
        compile_monaco(tmp_path, cancel=token)
    assert raised.value.kind == "cancelled"
    assert not (tmp_path / "monaco.pmtiles").exists()


def test_cancelling_from_the_progress_callback_stops_the_compile(tmp_path):
    token = otc.CancelToken()
    reports = []

    def progress(report):
        reports.append(report)
        token.cancel()

    with pytest.raises(otc.Cancelled):
        compile_monaco(tmp_path, progress=progress, cancel=token)
    assert len(reports) == 1
    assert not (tmp_path / "monaco.pmtiles").exists()


def test_a_callable_can_cancel(tmp_path):
    asked = []

    def cancel():
        asked.append(True)
        return True

    with pytest.raises(otc.Cancelled):
        compile_monaco(tmp_path, cancel=cancel)
    assert asked


def test_a_callable_returning_false_lets_the_compile_finish(tmp_path):
    result = compile_monaco(tmp_path, max_zoom=6, cancel=lambda: False)
    assert result["result"]["tiles"] > 0


def test_a_token_cancelled_from_another_thread_stops_the_compile(tmp_path):
    token = otc.CancelToken()
    started = threading.Event()

    def progress(report):
        started.set()
        # Hold the compile until the other thread has cancelled it.
        assert token_set.wait(5)

    token_set = threading.Event()

    def canceller():
        started.wait(5)
        token.cancel()
        token_set.set()

    thread = threading.Thread(target=canceller)
    thread.start()
    with pytest.raises(otc.Cancelled):
        compile_monaco(tmp_path, progress=progress, cancel=token)
    thread.join()


def test_an_exception_in_progress_stops_the_compile_and_is_raised(tmp_path):
    def progress(report):
        raise ValueError("stop here")

    with pytest.raises(ValueError, match="stop here"):
        compile_monaco(tmp_path, progress=progress)
    assert not (tmp_path / "monaco.pmtiles").exists()


def test_an_exception_in_cancel_is_raised(tmp_path):
    def cancel():
        raise RuntimeError("cancel broke")

    with pytest.raises(RuntimeError, match="cancel broke"):
        compile_monaco(tmp_path, cancel=cancel)


def test_cancel_and_progress_must_be_callable(tmp_path):
    with pytest.raises(TypeError, match="cancel"):
        compile_monaco(tmp_path, cancel=True)
    with pytest.raises(TypeError, match="progress"):
        compile_monaco(tmp_path, progress="loud")


def test_extract_can_be_cancelled(monaco, tmp_path):
    token = otc.CancelToken()
    token.cancel()
    with pytest.raises(otc.Cancelled):
        otc.extract(monaco, tmp_path / "cut.pmtiles", cancel=token)
    assert not (tmp_path / "cut.pmtiles").exists()


def test_a_keyboard_interrupt_in_a_callback_stops_the_compile(tmp_path):
    def progress(report):
        raise KeyboardInterrupt

    with pytest.raises(KeyboardInterrupt):
        compile_monaco(tmp_path, progress=progress)
    assert not (tmp_path / "monaco.pmtiles").exists()
