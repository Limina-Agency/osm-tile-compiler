import MapLibre
import SwiftUI

/// MapLibre Native showing the run's offline style pack, which points at the
/// archive with a `pmtiles://file://` URL. Reports how long the first full
/// render took.
struct MapView: UIViewRepresentable {
    var styleURL: URL
    var bounds: Bounds?
    var onRendered: (Double) -> Void
    var onFailed: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(bounds: bounds, onRendered: onRendered, onFailed: onFailed)
    }

    func makeUIView(context: Context) -> MLNMapView {
        context.coordinator.started = Date()
        let map = MLNMapView(frame: .zero, styleURL: styleURL)
        map.delegate = context.coordinator
        map.logoView.isHidden = true
        map.attributionButton.isHidden = false
        return map
    }

    func updateUIView(_ map: MLNMapView, context: Context) {}

    final class Coordinator: NSObject, MLNMapViewDelegate {
        let bounds: Bounds?
        let onRendered: (Double) -> Void
        let onFailed: (String) -> Void
        var started = Date()
        var reported = false

        init(bounds: Bounds?, onRendered: @escaping (Double) -> Void, onFailed: @escaping (String) -> Void) {
            self.bounds = bounds
            self.onRendered = onRendered
            self.onFailed = onFailed
        }

        func mapView(_ mapView: MLNMapView, didFinishLoading style: MLNStyle) {
            guard let bounds else { return }
            let box = MLNCoordinateBounds(
                sw: CLLocationCoordinate2D(latitude: bounds.south, longitude: bounds.west),
                ne: CLLocationCoordinate2D(latitude: bounds.north, longitude: bounds.east))
            mapView.setVisibleCoordinateBounds(box, edgePadding: .zero, animated: false, completionHandler: nil)
        }

        func mapViewDidFinishRenderingMap(_ mapView: MLNMapView, fullyRendered: Bool) {
            guard fullyRendered, !reported else { return }
            reported = true
            onRendered(Date().timeIntervalSince(started))
        }

        func mapViewDidFailLoadingMap(_ mapView: MLNMapView, withError error: Error) {
            onFailed(error.localizedDescription)
        }
    }
}
