// Dart (dart:ffi) example for libgrafile.
//
// Setup:
//   cd ffi/examples/dart && dart pub add ffi
// Build the library from the repository root:
//   cargo build -p grafile-ffi
// Run with the library on the dynamic loader path:
//   DYLD_LIBRARY_PATH=../../target/debug dart run example.dart   # macOS
//   LD_LIBRARY_PATH=../../target/debug dart run example.dart     # Linux
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

typedef _OpenNative = Int32 Function(
    Pointer<Utf8>, Pointer<Pointer<Void>>, Pointer<Pointer<Utf8>>);
typedef _OpenDart = int Function(
    Pointer<Utf8>, Pointer<Pointer<Void>>, Pointer<Pointer<Utf8>>);

typedef _ExecNative = Int32 Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>);
typedef _ExecDart = int Function(Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>);

typedef _QueryJsonNative = Int32 Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Pointer<Utf8>>);
typedef _QueryJsonDart = int Function(
    Pointer<Void>, Pointer<Utf8>, Pointer<Utf8>, Pointer<Pointer<Utf8>>);

typedef _ErrmsgNative = Pointer<Utf8> Function(Pointer<Void>);
typedef _ErrmsgDart = Pointer<Utf8> Function(Pointer<Void>);

typedef _FreeNative = Void Function(Pointer<Void>);
typedef _FreeDart = void Function(Pointer<Void>);

typedef _CloseNative = Void Function(Pointer<Void>);
typedef _CloseDart = void Function(Pointer<Void>);

void main() {
  final library = Platform.isMacOS
      ? DynamicLibrary.open('libgrafile.dylib')
      : DynamicLibrary.open('libgrafile.so');

  final open = library.lookupFunction<_OpenNative, _OpenDart>('grafile_open');
  final exec = library.lookupFunction<_ExecNative, _ExecDart>('grafile_exec');
  final queryJson =
      library.lookupFunction<_QueryJsonNative, _QueryJsonDart>(
          'grafile_query_json');
  final errmsg = library.lookupFunction<_ErrmsgNative, _ErrmsgDart>(
      'grafile_errmsg');
  final free = library.lookupFunction<_FreeNative, _FreeDart>('grafile_free');
  final close =
      library.lookupFunction<_CloseNative, _CloseDart>('grafile_close');

  final db = calloc<Pointer<Void>>();
  final err = calloc<Pointer<Utf8>>();
  final rc = open('dart-example.grafile'.toNativeUtf8(), db, err);
  if (rc != 0) {
    stderr.writeln('open failed: ${err.value.toDartString()}');
    exit(1);
  }

  final create = "CREATE (:Person {name: 'alice'})".toNativeUtf8();
  if (exec(db.value, create, nullptr) != 0) {
    stderr.writeln('exec failed: ${errmsg(db.value).toDartString()}');
    exit(1);
  }
  calloc.free(create);

  final query =
      "MATCH (n:Person) RETURN n.name AS name".toNativeUtf8();
  final out = calloc<Pointer<Utf8>>();
  if (queryJson(db.value, query, nullptr, out) != 0) {
    stderr.writeln('query failed: ${errmsg(db.value).toDartString()}');
    exit(1);
  }
  final rows = jsonDecode(out.value.toDartString());
  stdout.writeln(rows);
  free(out.value.cast());
  calloc.free(out);
  calloc.free(query);

  close(db.value);
  calloc.free(db);
  calloc.free(err);
}
