# Real extracts.
#
# Geofabrik keeps the January 1 extract of every year, but first-of-month
# extracts only for a few months and daily ones for about a week, so pin
# January 1 extracts. Each covers something the synthetic cases cannot
# fake convincingly.
{ fetchurl }:

let
  geofabrik =
    path: hash:
    fetchurl {
      urls = [ "https://download.geofabrik.de/${path}-260101.osm.pbf" ];
      inherit hash;
      # Geofabrik has outages of some minutes (HTTP 502): retry for up to 15
      # minutes, not fetchurl's 3 quick retries. A missing file waits as long.
      curlOptsList = [
        "--retry"
        "15"
        "--retry-delay"
        "60"
      ];
    };
in
{
  # A dense city state: the most data per tile, 0.7 MB.
  monaco = geofabrik "europe/monaco" "sha256-LA3r12dsDQ2A7FNCUcsASmeXRB0UUSy4PQH+GtgTY78=";

  # Alpine, small: the first real extract the pipeline was built on.
  liechtenstein = geofabrik "europe/liechtenstein" "sha256-OMKBkg3PWZdUoeWPOZuIw10YtA3/kyvU3ZJqoaec2nY=";

  # Islands on both sides of the antimeridian.
  fiji = geofabrik "australia-oceania/fiji" "sha256-lW4CrjbowSZbonfbAMclDEly1jhdpNWpKXRrffJOi5Q=";

  # The south pole, the antimeridian and continent-sized polygons.
  antarctica = geofabrik "antarctica" "sha256-1OZU3k+gVjuTbWaWTBwMejFeUE/HJ4B53r/YzbyglD0=";

  # A whole country at 46 MB: every file spans many blocks.
  luxembourg = geofabrik "europe/luxembourg" "sha256-2PPh2rtNnVSJUAw1Qy6zkwicMiIs//jPUHBoUvBpaoA=";
}
