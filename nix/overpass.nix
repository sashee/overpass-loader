# Overpass API built from the pinned upstream source. By default everything
# `make install` installs, which is what a server runs: `dispatcher`,
# `osm3s_query`, `update_database`, the `cgi-bin/interpreter`, scripts and
# templates; plus the rules in share/overpass/rules. `programs` builds only
# those binaries, for test variants; `patches` adds local fixes on top.
{
  lib,
  stdenv,
  autoreconfHook,
  expat,
  zlib,
  lz4,
  src,
  patches ? [ ],
  variant ? "upstream",
  programs ? null,
}:

stdenv.mkDerivation (
  {
    pname = "overpass-api-${variant}";
    version = "0.7.62.11";

    inherit src patches;
    # The autotools project lives in src/ of the upstream repository.
    sourceRoot = "source/src";

    nativeBuildInputs = [ autoreconfHook ];
    buildInputs = [
      expat
      zlib
      lz4
    ];
    # lz4 support must match the databases: a server built without it cannot
    # read lz4-compressed files.
    configureFlags = [ "--enable-lz4" ];
    # Autoconf defaults to "-g -O2". Debug info does not change the generated
    # code, but it roughly doubles the compiler's memory use on these sources
    # (about 1 GB per g++ process with it), so leave it out.
    env.CXXFLAGS = "-O2";

    enableParallelBuilding = true;

    postInstall = ''
      mkdir -p $out/share/overpass
      cp -r rules $out/share/overpass/
    '';

    passthru = { inherit variant; };

    meta = {
      description = "Overpass API: server, query and import programs (${variant})";
      homepage = "https://github.com/drolbr/Overpass-API";
      license = lib.licenses.agpl3Plus;
      platforms = lib.platforms.linux;
      mainProgram = "osm3s_query";
    };
  }
  // lib.optionalAttrs (programs != null) {
    buildFlags = map (p: "bin/${p}") programs;
    installPhase = ''
      runHook preInstall
      install -Dm755 -t $out/bin ${lib.concatMapStringsSep " " (p: "bin/${p}") programs}
      runHook postInstall
    '';
  }
)
