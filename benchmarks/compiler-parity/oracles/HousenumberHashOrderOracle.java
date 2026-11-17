import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/** Reproduce the default groupingBy HashMap iteration used by Housenumber.postProcess. */
public final class HousenumberHashOrderOracle {
  private HousenumberHashOrderOracle() {}

  public static void main(String[] args) throws Exception {
    if (args.length != 1) {
      throw new IllegalArgumentException("expected UTF-8 file with one partition per line");
    }
    List<String> partitions = Files.readAllLines(Path.of(args[0]), StandardCharsets.UTF_8);
    Map<String, List<Integer>> groups = IntStream.range(0, partitions.size())
      .boxed()
      .collect(Collectors.groupingBy(partitions::get));
    String order = groups.values().stream()
      .map(group -> Integer.toString(group.getFirst()))
      .collect(Collectors.joining(","));
    System.out.println(order);
  }
}
