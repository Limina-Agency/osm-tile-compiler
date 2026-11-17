import com.onthegomap.planetiler.geo.GeoUtils;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;

/** Probes an original near-half-grid vertex through pinned JTS float repair. */
public class FloatRepairProvenanceOracle {
  public static void main(String[] args) throws Exception {
    var shell = GeoUtils.JTS_FACTORY.createPolygon(new Coordinate[] {
        new Coordinate(0.4999999 / 16, 0),
        new Coordinate(4000.0 / 16, 3900.0 / 16),
        new Coordinate(4000.0 / 16, 0),
        new Coordinate(0, 4000.0 / 16),
        new Coordinate(0.4999999 / 16, 0)
    });
    Geometry floatFixed = shell.buffer(0);
    System.out.println("input_valid=" + shell.isValid());
    System.out.println("float_fixed=" + floatFixed.toText());
    for (var coordinate : floatFixed.getCoordinates()) {
      System.out.println("mvt_float=" + coordinate.x * 16 + "," + coordinate.y * 16
          + " mvt_snap=" + Math.round(coordinate.x * 16) + "," + Math.round(coordinate.y * 16));
    }
  }
}
