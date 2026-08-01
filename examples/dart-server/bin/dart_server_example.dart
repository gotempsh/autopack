// Uses only dart:io so the example exercises the compile step, not pub.
import 'dart:io';

Future<void> main() async {
  final port = int.parse(Platform.environment['PORT'] ?? '3000');
  final server = await HttpServer.bind(InternetAddress.anyIPv4, port);
  print('listening on $port');

  await for (final request in server) {
    request.response
      ..headers.contentType = ContentType.text
      ..write('hello from autopack\n');
    await request.response.close();
  }
}
