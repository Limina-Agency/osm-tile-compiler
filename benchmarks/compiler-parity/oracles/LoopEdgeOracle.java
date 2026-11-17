import com.onthegomap.planetiler.util.LoopLineMerger;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.GeometryFactory;
import org.locationtech.jts.geom.LineString;

/** Directed closed-loop regression against the pinned Planetiler 0.10.2 JAR. */
public final class LoopEdgeOracle {
  private static final GeometryFactory FACTORY = new GeometryFactory();

  private static LineString line(int... xy) {
    Coordinate[] coordinates = new Coordinate[xy.length / 2];
    for (int i = 0; i < coordinates.length; i++) {
      coordinates[i] = new Coordinate(xy[2 * i] / 16d, xy[2 * i + 1] / 16d);
    }
    return FACTORY.createLineString(coordinates);
  }

  private static void check(String name, String expected, LineString... paths) {
    LoopLineMerger merger = new LoopLineMerger()
      .setTolerance(0)
      .setMergeStrokes(true)
      .setMinLength(0)
      .setLoopMinLength(0)
      .setStubMinLength(0);
    for (LineString path : paths) {
      merger.add(path);
    }
    String actual = merger.getMergedLineStrings().toString();
    if (!actual.equals(expected)) {
      throw new AssertionError(name + ": expected " + expected + " but got " + actual);
    }
    System.out.println(name + " " + actual);
  }

  public static void main(String[] args) {
    LineString forward = line(0, 0, 16, 0, 16, 16, 0, 16, 0, 0);
    LineString reverse = line(0, 0, 0, 16, 16, 16, 16, 0, 0, 0);
    LineString branch = line(0, 0, -16, 0);
    check("single", "[LINESTRING (0 0, 1 0, 1 1, 0 1, 0 0)]", forward);
    check("reverse", "[LINESTRING (0 1, 0 0, 1 0, 1 1, 0 1)]", forward, reverse);
    check("loop_branch", "[LINESTRING (-1 0, 0 0, 1 0, 1 1, 0 1, 0 0)]", forward, branch);
    check("reverse_branch", "[LINESTRING (0 0, 0 1, 1 1, 1 0, 0 0, -1 0)]", forward, reverse, branch);
  }
}
