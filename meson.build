project('gst-plugins-rs',
        'rust',
        'c',
        version: '0.14.0-alpha.1',
        meson_version : '>= 1.1')

# dependencies.py needs a toml parsing module
api_version = '1.0'
python = import('python').find_installation(modules: ['tomllib'], required: false)
if not python.found()
  python = import('python').find_installation(modules: ['tomli'])
endif
fs = import('fs')
host_system = host_machine.system()

if get_option('debug')
  target = 'debug'
else
  target = 'release'
endif

cargo = find_program('cargo', version:'>=1.40')
cargo_wrapper = find_program('cargo_wrapper.py')
cargo_c = find_program('cargo-cbuild', version:'>=0.9.21', required: false)

rustc = meson.get_compiler('rust')

if not cargo_c.found()
  error('cargo-c missing, install it with: \'cargo install cargo-c\'')
endif

system = host_machine.system()
exe_suffix = ''
if system == 'windows'
  exe_suffix = '.exe'
  ext_dynamic = 'dll'
  ext_static = 'lib'
elif system == 'darwin'
  ext_dynamic = 'dylib'
  ext_static = 'a'
else
  ext_dynamic = 'so'
  ext_static = 'a'
endif

# Extra env to pass to cargo
extra_env = {}

# Used to not lookup the same dependency multiple times which clutters logs
deps_cache = {}

# Need to depends on all gstreamer-rs deps to ensure they are built
# before gstreamer-rs when building with gst-build.
# Custom targets can't depend on dependency() objects so we have to depend
# on the library variable from the subproject instead.
glib_req = '>=2.62'
gst_req = '>=1.20.0'
depends = []

deps = [
  # name, subproject name, subproject dep, library object
  ['gstreamer-1.0', 'gstreamer', 'gst_dep', 'libgst'],
  ['gstreamer-app-1.0', 'gst-plugins-base', 'app_dep', 'gstapp'],
  ['gstreamer-audio-1.0', 'gst-plugins-base', 'audio_dep', 'gstaudio'],
  ['gstreamer-base-1.0', 'gstreamer', 'gst_base_dep', 'gst_base'],
  ['gstreamer-video-1.0', 'gst-plugins-base', 'video_dep', 'gstvideo'],
]

webrtc_option = get_option('webrtc')
rtp_option = get_option('rtp').enable_if(webrtc_option.enabled(), error_message: 'webrtc option needs rtp')

rtsp_option = get_option('rtsp')

if get_option('threadshare').allowed() \
  or get_option('onvif').allowed() \
  or get_option('raptorq').allowed() \
  or rtp_option.allowed() \
  or webrtc_option.allowed()
  deps += [['gstreamer-rtp-1.0', 'gst-plugins-base', 'rtp_dep', 'gst_rtp']]
endif
if get_option('webrtc').allowed() \
  or get_option('webrtchttp').allowed()
  deps += [['gstreamer-webrtc-1.0', 'gst-plugins-bad', 'gstwebrtc_dep', 'gstwebrtc']]
  deps += [['gstreamer-sdp-1.0', 'gst-plugins-base', 'sdp_dep', 'gstsdp']]
endif
if get_option('tests').allowed()
  deps += [['gstreamer-check-1.0', 'gstreamer', 'gst_check_dep', 'gst_check']]
endif
if get_option('gtk4').allowed() or get_option('skia').allowed()
  deps += [['gstreamer-gl-1.0', 'gst-plugins-base', 'gstgl_dep', 'gstgl']]
endif
if get_option('threadshare').allowed() or get_option('rtsp').allowed()
  deps += [['gstreamer-net-1.0', 'gstreamer', 'gst_net_dep', 'gst_net']]
endif
if get_option('relationmeta').allowed()
  deps += [['gstreamer-analytics-1.0', 'gst-plugins-bad', 'gstanalytics_dep', 'gstanalytics']]
endif

glib_dep = dependency('glib-2.0', version: glib_req)
deps_cache += {'glib-2.0': glib_dep}

ges_dep = dependency('gst-editing-services-1.0', version: '>=1.24', required: get_option('with-ges'))

skia_features = []
if ges_dep.found()
  skia_features += 'ges'
endif

foreach d: deps
  dep = dependency(d[0], version: gst_req,
    fallback : [d[1], d[2]], required: d.get(4, true))
  set_variable(d[2], dep)
  deps_cache += {d[0]: dep}
  if dep.type_name() == 'internal'
    lib = subproject(d[1]).get_variable(d[3])
    depends += lib
  endif
endforeach

# kept in the same order as the `members` list in Cargo.toml
plugins = {
  'audiofx': {
    'library': 'libgstrsaudiofx',
    'examples': ['hrtfrender'],
  },
  'claxon': {'library': 'libgstclaxon'},
  # csound has a non-trivial external dependency, see below
  'lewton': {'library': 'libgstlewton'},
  'spotify': {'library': 'libgstspotify'},

  'uridecodepool': {
    'library': 'libgsturidecodepool',
    'extra-deps': {'gstreamer-validate-1.0': {'enabled-features': ['validate']}},
  },
  'segmentclipper': {'library': 'libgstsegmentclipper'},

  'file': {'library': 'libgstrsfile'},

  'ges': {
    'library': 'libgstrsges',
    'gst-version': '>=1.26',
    'extra-deps': {'gstreamer-validate-1.0': {'enabled-features': ['validate']}},
  },

  'originalbuffer': {'library': 'libgstoriginalbuffer'},
  # sodium can have an external dependency, see below
  'threadshare': {
    'library': 'libgstthreadshare',
    'examples': [
      'ts-benchmark',
      'udpsrc-benchmark-sender',
      'tcpclientsrc-benchmark-sender',
      'ts-standalone',
    ],
  },
  'inter': {'library': 'libgstrsinter'},
  'streamgrouper': {'library': 'libgststreamgrouper'},

  'mp4': {'library': 'libgstmp4'},
  'fmp4': {
    'library': 'libgstfmp4',
    'examples': [
      'dash_vod',
      'hls_live',
      'hls_vod',
    ],
  },

  'aws': {
    'library': 'libgstaws',
    'extra-deps': {'openssl': {'version': ['>=1.1']}},
  },
  'mpegtslive': {'library': 'libgstmpegtslive'},
  'hlsmultivariantsink': {'library': 'libgsthlsmultivariantsink'},
  'hlssink3': {'library': 'libgsthlssink3'},
  'ndi': {'library': 'libgstndi'},
  'onvif': {
    'library': 'libgstrsonvif',
    'extra-deps': {'pangocairo': {}},
  },
  'raptorq': {'library': 'libgstraptorq'},
  'reqwest': {'library': 'libgstreqwest'},
  'relationmeta': {'library': 'libgstrsrelationmeta'},
  'rtsp': {'library': 'libgstrsrtsp'},
  'rtp': {'library': 'libgstrsrtp'},
  'webrtchttp': {'library': 'libgstwebrtchttp'},
  'webrtc': {
    'library': 'libgstrswebrtc',
    'examples': ['webrtcsink-stats-server'],
    'option-features': ['aws', 'livekit']
  },

  'textahead': {'library': 'libgsttextahead'},
  'json': {'library': 'libgstjson'},
  'regex': {'library': 'libgstregex'},
  'textwrap': {'library': 'libgsttextwrap'},

  'tracers': {
    'library': 'libgstrstracers',
    'extra-deps': {'gst-editing-services-1.0': {'enabled-features': ['ges']}},
  },
  'uriplaylistbin': {
    'library': 'libgsturiplaylistbin',
    'examples': ['playlist'],
    'features': ['clap'],
    'gst-version': '>=1.23.90',
  },

  'cdg': {'library': 'libgstcdg'},
  'closedcaption': {
    'library': 'libgstrsclosedcaption',
    'extra-deps': {
      'pango': {},
      'pangocairo': {},
      'cairo-gobject': {},
    }
  },
  'dav1d': {
    'library': 'libgstdav1d',
    'extra-deps': {'dav1d': {'version': ['>=1.3']}},
  },
  'ffv1': {'library': 'libgstffv1'},
  'flavors': {'library': 'libgstrsflv'},
  'gif': {
    'library': 'libgstgif',
    'examples': ['testvideosrc2gif'],
  },
  # gtk4 is added below
  'hsv': {'library': 'libgsthsv'},
  'png': {
    'library': 'libgstrspng',
    'examples': ['pngenc'],
  },
  'rav1e': {'library': 'libgstrav1e'},
  'videofx': {
    'library': 'libgstrsvideofx',
    'extra-deps': {'cairo-gobject': {}},
  },
  'gopbuffer': {'library': 'libgstgopbuffer'},
  'quinn': {
    'library': 'libgstquinn',
    'examples': [
      'quic_mux',
      'quic_roq',
    ],
  },
  'skia': {
    'library': 'libgstskia',
    'features': skia_features,
  },
  'validate': {
    'library': 'libgstrsvalidate',
    }, # Only used to handle the validate feature option
  'speechmatics': {'library': 'libgstspeechmatics'},
  'vvdec': {
    'library': 'libgstvvdec',
    'extra-deps': {'libvvdec': {'version': ['>= 3.0']}}
  }
}

# Won't build on platforms where it bundles the sources because of:
# https://github.com/qnighy/libwebp-sys2-rs/issues/12
# the fix is:
# https://github.com/qnighy/libwebp-sys2-rs/pull/13
if host_system not in ['windows', 'darwin']
  # FIXME: libwebp-sys2 will build its bundled version on msvc and apple platforms
  # https://github.com/qnighy/libwebp-sys2-rs/issues/4
  plugins += {'webp': {
    'library': 'libgstrswebp',
    'extra-deps': {'libwebpdemux': {}},
  }}
endif

sodium_opt = get_option('sodium')
if sodium_opt.allowed()
  sodium_plugin = {'sodium': {
    'library': 'libgstsodium',
    'examples': ['generate-keys', 'encrypt-example', 'decrypt-example'],
    'features': ['serde', 'serde_json', 'clap'],
  }}
  if get_option('sodium-source') == 'system'
    sodium_dep = dependency('libsodium', required: sodium_opt.enabled())
    extra_env += {'SODIUM_USE_PKG_CONFIG': '1'}
    if sodium_dep.found()
      plugins += sodium_plugin
    endif
  else
    plugins += sodium_plugin
  endif
endif

ges_option = get_option('ges')
if ges_option.allowed()
  deps += [['gstreamer-editing-services-1.0', 'gst-editing-services', 'ges_dep', 'libges', ges_option]]
endif

skia_opt = get_option('skia')
if skia_opt.allowed()
  # Try to use Skia from Meson subproject/wrap
  skia_dep_from_meson = dependency('skia', required: false, allow_fallback: true,
    default_options: ['gl=enabled', 'textlayout=enabled'])
  if skia_dep_from_meson.found() and skia_dep_from_meson.type_name() == 'internal'
    message('Using Skia from Meson')
    extra_env += {'SKIA_USE_PKG_CONFIG': '1'}
  endif
  if host_system == 'emscripten'
    # bindgen uses its own clang which doesn't get emscripten's implicit
    # -iwithsysroot/include/compat. Add it so xlocale.h etc. are found.
    emcc_cflags = run_command(
      'emcc', '--cflags',
      capture: true, check: true,
    ).stdout().strip()
    foreach flag : emcc_cflags.split()
      if flag.startswith('--sysroot=')
        sysroot = flag.split('=')[1]
        extra_env += {
          'BINDGEN_EXTRA_CLANG_ARGS': '--sysroot=' + sysroot + ' -iwithsysroot/include/compat',
        }
        break
      endif
    endforeach
    # skia-bindings uses cc::Build to compile C++ shims. These must be
    # compiled with atomics+bulk-memory to match the pthreads build.
    extra_env += {
      'CXXFLAGS': '-pthread -matomics -mbulk-memory -fPIC',
    }
  endif
endif

cc = meson.get_compiler('c')
csound_option = get_option('csound')
if csound_option.allowed()
  # if csound isn't distributed with pkg-config then user needs to define CSOUND_LIB_DIR with its location

  res = run_command(python, '-c', 'import os; print(os.environ["CSOUND_LIB_DIR"])', check: false)
  if res.returncode() == 0
    csound_libdir = res.stdout().strip()
    csound_dep = cc.find_library('csound64', dirs: csound_libdir, required: false)
    if csound_dep.found()
      plugins += {'csound': {
        'library': 'libgstcsound',
        'examples': ['csound-effect'],
      }}
      extra_env += {'CSOUND_LIB_DIR': csound_libdir}
    elif csound_option.enabled()
      error('csound option is enabled, but csound64 library could not be found and CSOUND_LIB_DIR was not set')
    endif
  endif
endif

if get_option('gtk4').allowed()
  gtk4_features = []
  gl_winsys = gstgl_dep.get_variable('gl_winsys').split()
  gl_platforms = gstgl_dep.get_variable('gl_platforms').split()
  if 'wayland' in gl_winsys
    gtk4_features += 'wayland'
  endif
  if 'x11' in gl_winsys
    if 'egl' in gl_platforms
      gtk4_features += 'x11egl'
    endif
    if 'glx' in gl_platforms
      gtk4_features += 'x11glx'
    endif
  elif host_system == 'windows'
    if 'egl' in gl_platforms
      gtk4_features += 'winegl'
    endif
  endif

  gst_allocators_dep = dependency('gstreamer-allocators-1.0', version: '>=1.24', required: false)
  gtk_dep = dependency('gtk4', version: '>=4.6', required: get_option('gtk4'))
  if gtk_dep.found()
    if host_system == 'linux' and gtk_dep.version().version_compare('>=4.14') and \
        gst_allocators_dep.found() and 'wayland' in gtk4_features
      gtk4_features += 'dmabuf'
    endif

    if gtk_dep.version().version_compare('>=4.16')
      gtk4_features += 'gtk_v4_16'
    elif gtk_dep.version().version_compare('>=4.14')
      gtk4_features += 'gtk_v4_14'
    elif gtk_dep.version().version_compare('>=4.12')
      gtk4_features += 'gtk_v4_12'
    elif gtk_dep.version().version_compare('>=4.10')
      gtk4_features += 'gtk_v4_10'
    endif

    plugins += {
      'gtk4': {
        'library': 'libgstgtk4',
        'examples': ['gtksink'],
        'extra-deps': {'gtk4': {'version': ['>=4.6']}},
        'features': gtk4_features,
      },
    }
  endif
endif

examples_opt = get_option('examples')
if examples_opt.allowed() and 'gtk4' in plugins
  plugins += {
    'fallbackswitch': {
      'library': 'libgstfallbackswitch',
      'examples_features': {
        'gtk-fallbackswitch': ['gtk', 'gio', 'gst-plugin-gtk4'],
      },
    },
    'livesync': {
      'library': 'libgstlivesync',
      'examples_features': {
        'gtk-livesync': ['gtk', 'gio', 'gst-plugin-gtk4'],
      }
    },
    'togglerecord': {
      'library': 'libgsttogglerecord',
      'examples_features': {
        'gtk-recording': ['gtk', 'gio', 'gst-plugin-gtk4'],
      }
    },
  }
else
  plugins += {
    'fallbackswitch': { 'library': 'libgstfallbackswitch'},
    'livesync': { 'library': 'libgstlivesync'},
    'togglerecord': { 'library': 'libgsttogglerecord'},
  }
endif

# Process plugins list

default_library = get_option('default_library')

library_suffixes = []
if default_library in ['shared', 'both']
  library_suffixes += [ext_dynamic]
endif
if default_library in ['static', 'both']
  library_suffixes += [ext_static]
endif

# cargo packages (plugins) to build
packages = []
# cargo features
features = []
# examples to build
examples = []
# Add the plugin library and .pc files as output
output = []
install_dirs = []

auto_plugin_features = get_option('auto_plugin_features')
gtk4_opt = get_option('gtk4')
if gtk4_opt.auto()
  gtk4_opt = auto_plugin_features
endif
if gtk4_opt.allowed()
  if glib_dep.version().version_compare('>=2.74')
    features += ['glib/v2_74', 'gio/v2_74']
  elif glib_dep.version().version_compare('>=2.72')
    features += ['glib/v2_72', 'gio/v2_72']
  elif glib_dep.version().version_compare('>=2.70')
    features += ['glib/v2_70', 'gio/v2_70']
  elif glib_dep.version().version_compare('>=2.68')
    features += ['glib/v2_68', 'gio/v2_68']
  elif glib_dep.version().version_compare('>=2.66')
    features += ['glib/v2_66', 'gio/v2_66']
  elif glib_dep.version().version_compare('>=2.64')
    features += ['glib/v2_64', 'gio/v2_64']
  elif glib_dep.version().version_compare('>=2.62')
    features += ['glib/v2_62', 'gio/v2_62']
  elif glib_dep.version().version_compare('>=2.60')
    features += ['glib/v2_60', 'gio/v2_60']
  elif glib_dep.version().version_compare('>=2.58')
    features += ['glib/v2_58', 'gio/v2_58']
  endif
endif

rav1e_opt = get_option('rav1e')
if rav1e_opt.auto()
  rav1e_opt = auto_plugin_features
endif
if rav1e_opt.allowed()
  nasm = find_program('nasm', required: false)
  if nasm.found()
    features += 'gst-plugin-rav1e/asm'
    extra_env += {'NASM': nasm.full_path()}
  endif
endif


if get_option('default_library') == 'static'
  extra_env += {
    # Tell the pkg-config crate to think of all libraries as static
    'PKG_CONFIG_ALL_STATIC': '1',
    # Tell the system-deps crate to process linker flag for static deps
    'SYSTEM_DEPS_LINK': 'static'
  }
endif

plugins_install_dir = get_option('libdir') / 'gstreamer-1.0'
pkgconfig_install_dir = get_option('libdir') / 'pkgconfig'

foreach plugin_name, details: plugins
  plugin_opt = get_variable(f'@plugin_name@_option', get_option(plugin_name))
  if plugin_opt.auto()
    plugin_opt = auto_plugin_features
  endif
  if not plugin_opt.allowed()
    debug(f'@plugin_name@ is disabled')
    continue
  endif
  plugin_deps_found = true

  # Check whether we have all needed deps
  foreach dep_name, dep_config: details.get('extra-deps', {})
    dep_ver = dep_config.get('version', [])

    # Determine if this dependency is required or optional
    if dep_config.has_key('enabled-features')
      # Optional dependency that enables features
      if dep_config.has_key('option-name')
        dep_required = get_option(dep_config['option-name'])
      else
        dep_required = false
      endif
    else
      # Required dependency for the plugin
      dep_required = plugin_opt
    endif

    if dep_ver.length() != 0
      dep = dependency(dep_name, version: dep_ver, required: dep_required)
    else
      dep = dependency(dep_name, required: dep_required)
    endif
    deps_cache += {dep_name: dep}

    if not dep.found()
      if dep_config.has_key('enabled-features')
        # Optional dependency not found, skip features but continue with plugin
        if dep_ver.length() != 0
          dep_ver_msg = ' '.join(dep_ver)
          debug(f'@plugin_name@ optional dependency @dep_name@ @dep_ver_msg@ not found, skipping features')
        else
          debug(f'@plugin_name@ optional dependency @dep_name@ not found, skipping features')
        endif
      else
        # Required dependency not found, skip entire plugin
        if dep_ver.length() != 0
          dep_ver_msg = ' '.join(dep_ver)
          debug(f'@plugin_name@ dependency @dep_name@ @dep_ver_msg@ not found, skipping')
        else
          debug(f'@plugin_name@ dependency @dep_name@ not found, skipping')
        endif
        plugin_deps_found = false
        break
      endif
    else
      # Add conditional features if dependency is found and has enabled-features
      foreach feature_name: dep_config.get('enabled-features', [])
        features += f'gst-plugin-@plugin_name@/@feature_name@'
      endforeach
    endif
  endforeach
  if not plugin_deps_found
    continue
  endif

  # Validate gst-plugin features
  plugin_features = details.get('features', [])
  foreach feature: plugin_features
    if feature.startswith('gst-plugin') and not packages.contains(feature)
      msg = f'@plugin_name@ required feature @feature@ not found'
      if plugin_opt.enabled()
        error(msg)
      endif
      message(msg + ', skipping')
      plugin_deps_found = false
      break
    endif
  endforeach
  if not plugin_deps_found
    continue
  endif

  option_features = []
  foreach feature : details.get('option-features', [])
    if get_option(f'@plugin_name@-@feature@').allowed()
      message(f'Enabling @feature@ for @plugin_name@')
      features += [feature]
    endif
  endforeach

  # Check if we have the required GStreamer version
  if details.has_key('gst-version') and not \
      deps_cache['gstreamer-1.0'].version().version_compare(details['gst-version'])
    msg = '@0@ requires gstreamer version @1@'.format(plugin_name, details['gst-version'])
    if plugin_opt.enabled()
      error(msg)
    endif
    message(msg + ', skipping')
    continue
  endif

  # Parse and enable examples
  plugin_examples = details.get('examples', [])
  foreach example: plugin_examples
    examples += example
  endforeach

  plugin_examples_features = details.get('examples_features', {})
  foreach example, examples_features: plugin_examples_features
    example_deps_found = true
    foreach feature: examples_features
      if feature.startswith('gst-plugin') and not packages.contains(feature)
        msg = f'@plugin_name@ example @example@ required feature @feature@ not found'
        if plugin_opt.enabled() and examples_opt.enabled()
          error(msg)
        endif
        message(msg + ', skipping')
        example_deps_found = false
        break
      endif
    endforeach
    features += examples_features
    if example_deps_found
      examples += example
    endif
  endforeach

  packages += f'gst-plugin-@plugin_name@'
  features += plugin_features
  extra_features = run_command('dependencies.py', meson.current_source_dir(), plugin_name,
    '--feature', '--gst-version', gst_dep.version(), capture: true, check: true).stdout().strip()
  if extra_features != ''
    features += extra_features.split(',')
  endif

  lib = details.get('library')
  output += [lib.substring(3) + '.pc']
  # No 'lib' suffix with MSVC
  if cc.get_argument_syntax() == 'msvc'
    lib = lib.substring(3)
  endif
  if default_library in ['shared', 'both']
    output += [lib + '.' + ext_dynamic]
  endif
  if default_library in ['static', 'both']
    output += [lib + '.' + ext_static]
  endif
  install_dirs += [pkgconfig_install_dir]
  install_dirs += [plugins_install_dir]

endforeach

feature_args = []
if features.length() > 0
  feature_args += ['--features', features]
endif

extra_args = []
if get_option('doc').disabled()
  extra_args += ['--disable-doc']
endif
if default_library == 'static'
  extra_args += ['--library-type', 'staticlib']
elif default_library == 'shared'
  extra_args += ['--library-type', 'cdylib']
endif

# cargo and system-deps run on the build machine, so we need the native
# pkg-config — not the cross-file override (which may be set to 'false'
# to prevent meson from finding host-system packages for C builds).
pkg_config = find_program('pkgconfig', 'pkg-config', native: true, required: false)
if pkg_config.found()
  extra_env += {'PKG_CONFIG': pkg_config.full_path()}
endif

pkg_config_path = get_option('pkg_config_path')
if pkg_config_path.length() > 0
  pathsep = ':'
  if host_system == 'windows'
    pathsep = ';'
  endif
  extra_env += {'PKG_CONFIG_PATH': pathsep.join(pkg_config_path)}
endif

# get cmdline for rust
rustc_cmd = ' '.join(rustc.cmd_array())
if host_system == 'emscripten'
  # Emscripten with pthreads needs atomics and bulk-memory features.
  # Use emscripten-style exceptions (-fexceptions) to match the C build.
  # Also need to rebuild std with atomics support (-Zbuild-std on nightly).
  # Meson doesn't reliably pass --target from cross-files for Rust, so ensure it.
  if not rustc_cmd.contains('--target')
    rustc_cmd += ' --target wasm32-unknown-emscripten'
  endif
  rustc_cmd += ' -C linker=emcc -C target-feature=+atomics,+bulk-memory -C link-arg=-fexceptions -C link-arg=-pthread'
  rustc_cmd += ' --cfg tokio_unstable'
  extra_env += {'RUSTUP_TOOLCHAIN': 'nightly'}
  rustc_cmd += ' -C panic=abort'
  extra_args += ['-Zbuild-std=std,panic_abort']
endif
extra_env += {'RUSTC': rustc_cmd}

# Build plugins in the plugins subdirectory
subdir('plugins')

# Build validate plugins in separate directory
subdir('validate-plugins')

# This is used by GStreamer to static link Rust plugins into gst-full
gst_plugins = []
plugin_names = []
foreach plugin : plugins
  plugin_name = fs.stem(plugin.full_path())
  # skip the 'lib' prefix from plugin path when not building with MSVC
  if cc.get_argument_syntax() != 'msvc'
    plugin_name = plugin_name.substring(3)
  endif

  plugin_display_name = plugin_name
  if plugin_name.startswith('gst')
    plugin_display_name = plugin_name.substring(3)
  endif
  if plugin_display_name in plugin_names
    # When default_library=both plugins are duplicated.
    continue
  endif
  plugin_names += plugin_display_name

  option_name = plugin_name.substring(3)
  if option_name.startswith('rs')
    option_name = option_name.substring(2)
  endif
  if option_name == 'flv'
    option_name = 'flavors'
  endif
  if not get_option(option_name).allowed()
    continue
  endif

  # Extract plugin dependencies from their Cargo.toml file
  plugin_deps = []
  p = run_command('dependencies.py', meson.current_source_dir(), plugin_name,
    capture: true,
    check: true)
  foreach dep_name : p.stdout().strip().split(',')
    dep_name_version = dep_name.split('|')
    dep_name = dep_name_version.get(0).strip()
    if dep_name_version.length() > 1
      extras = {'version': dep_name_version.get(1).strip()}
    else
      extras = {}
    endif
    if deps_cache.has_key(dep_name)
      plugin_deps += deps_cache[dep_name]
    else
      dep = dependency(dep_name, required: false, kwargs: extras)
      plugin_deps += dep
      deps_cache += {dep_name: dep}
    endif
  endforeach

  extra_link_args = []
  if plugin_name == 'gstskia' and skia_opt.allowed() and skia_dep_from_meson.found() and skia_dep_from_meson.type_name() == 'internal'
    # Skia is built as a custom_target which can't be propagated via
    # link_with through dependency chains. Pass library paths as link_args.
    skia_builddir = meson.global_build_root() / 'subprojects' / 'skia'
    extra_link_args = [skia_builddir / 'libskia.a']
  endif

  dep = declare_dependency(
    link_with: plugin,
    dependencies: plugin_deps,
    link_args: extra_link_args,
    variables: {'full_path': plugin.full_path()},
  )
  # validate plugins are already overridden in validate-plugins/meson.build
  if plugin_name != 'gstrsvalidate'
    meson.override_dependency(plugin_name, dep)
  endif

  if default_library == 'static' and plugin_name in ['gstcsound', 'gstthreadshare', 'gstgtk4']
    warning('Static plugin @0@ is known to fail. It will not be included in libgstreamer-full.'.format(plugin_name))
  else
    gst_plugins += dep
  endif
endforeach

# subdir('docs')

if get_option('webrtc').allowed()
  custom_target('gst-webrtc-signalling-server',
    build_by_default: true,
    output: 'gst-webrtc-signalling-server' + exe_suffix,
    console: true,
    install: true,
    install_dir: get_option('bindir'),
    depfile: 'gst-webrtc-signalling-server.dep',
    env: extra_env,
    command: [cargo_wrapper,
      'build',
      meson.current_build_dir(),
      meson.current_source_dir(),
      meson.global_build_root(),
      target,
      get_option('prefix'),
      get_option('libdir'),
      '--depfile', '@DEPFILE@',
      '--bin', 'gst-webrtc-signalling-server',
      '--exe-suffix', exe_suffix,
    ])
endif

if get_option('examples').allowed() and examples.length() > 0
  outputs = []
  foreach example: examples
    outputs += [example + exe_suffix]
  endforeach
  custom_target('gst-plugins-rs-examples',
    build_by_default: true,
    output: outputs,
    console: true,
    install: false,
    depfile: 'gst-plugins-rs-examples.dep',
    env: extra_env,
    command: [cargo_wrapper,
      'build',
      meson.current_build_dir(),
      meson.current_source_dir(),
      meson.global_build_root(),
      target,
      get_option('prefix'),
      get_option('libdir'),
      '--depfile', '@DEPFILE@',
      '--packages', packages,
      '--examples', examples,
      '--exe-suffix', exe_suffix,
    ] + feature_args)
endif

if get_option('tests').allowed()
  foreach package: packages
    test(package.replace('gst-plugin-', ''),
      cargo_wrapper,
      env: extra_env,
      args: ['test',
        meson.current_build_dir(),
        meson.current_source_dir(),
        meson.global_build_root(),
        target,
        get_option('prefix'),
        get_option('libdir'),
        '--packages', package],
      timeout: 600)
  endforeach
endif

summary({
    'Plugins': plugin_names,
}, list_sep: ', ')
