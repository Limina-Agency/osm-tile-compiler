import com.onthegomap.planetiler.util.LoopLineMerger;
import java.util.Arrays;
import java.util.List;
import org.locationtech.jts.geom.Coordinate;
import org.locationtech.jts.geom.GeometryFactory;
import org.locationtech.jts.geom.LineString;

/** Small, independently runnable probes against the pinned Planetiler JAR. */
public final class LoopLineMergerOracle {
  private static final GeometryFactory FACTORY = new GeometryFactory();
  private static final double SCALE = 16d;

  private LoopLineMergerOracle() {}

  private static Coordinate[] coordinates(int[][] scaled) {
    return Arrays.stream(scaled)
      .map(xy -> new Coordinate(xy[0] / SCALE, xy[1] / SCALE))
      .toArray(Coordinate[]::new);
  }

  private static String scaledCoordinates(LineString line) {
    return Arrays.toString(Arrays.stream(line.getCoordinates())
      .map(c -> "[" + Math.round(c.x * SCALE) + "," + Math.round(c.y * SCALE) + "]")
      .toArray(String[]::new));
  }

  private static void assertOutput(String name, List<LineString> lines, String expected) {
    String actual = lines.size() == 1 ? scaledCoordinates(lines.getFirst()) : "count=" + lines.size();
    if (!actual.equals(expected)) {
      throw new AssertionError(name + " expected " + expected + " but got " + actual);
    }
    System.out.println(name + " " + actual);
  }

  private static void actualTransportationPath() {
    // OSM way 176240791, transportation feature 1762407912, tile z14/9065/5555.
    int[][] scaled = {
      {4160, -1}, {4120, 20}, {4075, 41}, {4061, 49}, {4049, 57}, {4045, 65},
      {4045, 74}, {4056, 88}, {4074, 89}, {4088, 78}, {4101, 58}, {4120, 20}
    };
    LoopLineMerger merger = new LoopLineMerger()
      .setTolerance(1d / SCALE)
      .setMergeStrokes(true)
      .setMinLength(0)
      .setLoopMinLength(0)
      .setStubMinLength(0);
    merger.add(FACTORY.createLineString(coordinates(scaled)));
    assertOutput("transportation1762407912", merger.getMergedLineStrings(),
      "[[4160,-1], [4120,20], [4075,41], [4049,57], [4045,65], [4045,74], " +
      "[4056,88], [4074,89], [4088,78], [4101,58], [4120,20]]");
  }

  private static void halfTie(String name, double x, int expectedX) {
    LoopLineMerger merger = new LoopLineMerger()
      .setTolerance(0)
      .setMergeStrokes(true)
      .setMinLength(0)
      .setLoopMinLength(0)
      .setStubMinLength(0);
    merger.add(FACTORY.createLineString(new Coordinate[] {
      new Coordinate(x, 0), new Coordinate(x, 1)
    }));
    assertOutput(name, merger.getMergedLineStrings(),
      "[[" + expectedX + ",0], [" + expectedX + ",16]]");
  }

  private static void lengthThreshold(String name, double minLengthScaled, boolean survives) {
    Coordinate[] input = coordinates(new int[][] {{0, 0}, {10, 1}, {20, 0}});
    LoopLineMerger merger = new LoopLineMerger()
      .setTolerance(1.6d / SCALE)
      .setMergeStrokes(true)
      .setMinLength(minLengthScaled / SCALE)
      .setLoopMinLength(0)
      .setStubMinLength(0);
    merger.add(FACTORY.createLineString(input));
    assertOutput(name, merger.getMergedLineStrings(),
      survives ? "[[0,0], [20,0]]" : "count=0");
  }

  public static void main(String[] args) {
    actualTransportationPath();
    halfTie("half_tie_positive", 0.03125, 1);
    halfTie("half_tie_negative", -0.03125, 0);
    lengthThreshold("length_min_20_05", 20.05, true);
    lengthThreshold("length_min_20_2", 20.2, false);
  }
}
