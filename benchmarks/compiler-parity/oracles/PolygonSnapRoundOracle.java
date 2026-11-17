import com.fasterxml.jackson.databind.ObjectMapper;
import com.onthegomap.planetiler.geo.GeoUtils;
import com.onthegomap.planetiler.stats.Stats;
import java.nio.file.Path;
import org.locationtech.jts.geom.Coordinate;

/** Pinned precision-repair probe on an actual clipped farmland shell. */
public class PolygonSnapRoundOracle {
  public static void main(String[] args) throws Exception {
    var points = new ObjectMapper().readTree(Path.of(args[0]).toFile())
        .get("clipped_mvt_float_exterior");
    var coordinates = new Coordinate[points.size()];
    for (int i = 0; i < points.size(); i++) {
      coordinates[i] = new Coordinate(points.get(i).get(0).asDouble() / 16,
          points.get(i).get(1).asDouble() / 16);
    }
    var polygon = GeoUtils.JTS_FACTORY.createPolygon(coordinates);
    var fixed = GeoUtils.snapAndFixPolygon(polygon, Stats.inMemory(), "fixture");
    System.out.println("input_valid=" + polygon.isValid());
    for (var coordinate : fixed.getCoordinates()) {
      System.out.println(Math.round(coordinate.x * 16) + "," + Math.round(coordinate.y * 16));
    }
  }
}
