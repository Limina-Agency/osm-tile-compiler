import java.nio.file.Files;
import java.nio.file.Path;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.Geometry;
import org.locationtech.jts.geom.PrecisionModel;
import org.locationtech.jts.geom.util.AffineTransformation;
import org.locationtech.jts.precision.GeometryPrecisionReducer;
import com.onthegomap.planetiler.geo.DouglasPeuckerSimplifier;
import com.onthegomap.planetiler.geo.GeoUtils;

/** Replays a tracked OSM ring through the pinned projection and polygon repair. */
public class PolygonRepairOracle {
  public static void main(String[] args) throws Exception {
    int zoom = args.length > 1 ? Integer.parseInt(args[1]) : 10;
    int tileX = args.length > 2 ? Integer.parseInt(args[2]) : 572;
    int tileY = args.length > 3 ? Integer.parseInt(args[3]) : 349;
    var lines = Files.readAllLines(Path.of(args[0]));
    Coordinate[] coordinates = new Coordinate[lines.size()];
    for (int i = 0; i < lines.size(); i++) {
      String[] point = lines.get(i).split(",");
      long packed = GeoUtils.encodeFlatLocation(
          Double.parseDouble(point[0]), Double.parseDouble(point[1]));
      coordinates[i] = new Coordinate(GeoUtils.decodeWorldX(packed), GeoUtils.decodeWorldY(packed));
    }
    Geometry world = GeoUtils.JTS_FACTORY.createPolygon(coordinates);
    Geometry zoomed = AffineTransformation.scaleInstance(1 << zoom, 1 << zoom).transform(world);
    Geometry simplified = DouglasPeuckerSimplifier.simplify(zoomed, 0.25 / 256);
    Geometry tile = AffineTransformation.translationInstance(-tileX, -tileY).transform(simplified);
    tile = AffineTransformation.scaleInstance(4096, 4096).transform(tile);
    System.out.println("float_valid=" + tile.isValid());
    System.out.println("tile=" + tile.toText());
    try {
      Geometry reduced = GeometryPrecisionReducer.reduce(tile, new PrecisionModel(1));
      System.out.println("reduce_without_repair=" + reduced.toText());
    } catch (IllegalArgumentException error) {
      System.out.println("reduce_without_repair=" + error.getClass().getSimpleName());
    }
    Geometry fixed = tile.buffer(0);
    Geometry reduced = GeometryPrecisionReducer.reduce(fixed, new PrecisionModel(1));
    System.out.println("fixed_then_reduced=" + reduced.toText());
    System.out.println("area=" + reduced.getArea());
  }
}
