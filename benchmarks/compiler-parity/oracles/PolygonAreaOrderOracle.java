import com.onthegomap.planetiler.geo.GeoUtils;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;

/** Probes snap-before-area-filter ordering in pinned FeatureMerge.extractPolygons. */
public class PolygonAreaOrderOracle {
  public static void main(String[] args) throws Exception {
    var polygon = GeoUtils.JTS_FACTORY.createPolygon(new Coordinate[] {
        new Coordinate(0, 0),
        new Coordinate(16.01 / 16, 0),
        new Coordinate(16.01 / 16, 16.01 / 16),
        new Coordinate(0, 16.01 / 16),
        new Coordinate(0, 0)
    });
    Geometry snapped = GeoUtils.snapAndFixPolygon(polygon, null, "area-order-oracle");
    System.out.println("float_pixels_squared=" + polygon.getArea());
    System.out.println("snapped_pixels_squared=" + snapped.getArea());
    System.out.println("float_passes_min1=" + (polygon.getArea() > 1));
    System.out.println("snapped_passes_min1=" + (snapped.getArea() > 1));
  }
}
