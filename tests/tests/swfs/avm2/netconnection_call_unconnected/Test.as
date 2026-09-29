package {
	import flash.net.NetConnection;
	import flash.net.Responder;

	public class Test {
		public static function tryCall(label:String, c:NetConnection, withResponder:Boolean):void {
			try {
				var r:* = withResponder
					? c.call("test.method", new Responder(function(res:*):void { trace(label + " -> onResult " + res); },
						function(st:*):void { trace(label + " -> onStatus " + st); }))
					: c.call("test.method", null, "arg");
				trace(label + ": returned " + r + ", connected = " + c.connected);
			} catch (e:Error) {
				trace(label + ": threw " + e);
			}
		}
	}
}

var never:NetConnection = new NetConnection();
Test.tryCall("never connected", never, false);
Test.tryCall("never connected, with responder", never, true);

var closed:NetConnection = new NetConnection();
closed.connect(null);
closed.close();
Test.tryCall("connect(null) then close()", closed, false);
Test.tryCall("connect(null) then close(), with responder", closed, true);
trace("done");
