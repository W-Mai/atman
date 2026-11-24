package run.atman.mobile;

import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.util.Base64;
import androidx.activity.result.ActivityResult;
import com.getcapacitor.JSObject;
import com.getcapacitor.Plugin;
import com.getcapacitor.PluginCall;
import com.getcapacitor.PluginMethod;
import com.getcapacitor.annotation.ActivityCallback;
import com.getcapacitor.annotation.CapacitorPlugin;
import com.journeyapps.barcodescanner.ScanIntentResult;
import com.journeyapps.barcodescanner.ScanOptions;
import java.io.IOException;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.util.HashMap;
import java.util.Locale;
import java.util.Map;
import java.util.UUID;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.TimeUnit;
import okhttp3.Call;
import okhttp3.Callback;
import okhttp3.HttpUrl;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.RequestBody;
import okhttp3.Response;
import org.json.JSONObject;

@CapacitorPlugin(name = "AtmanDevice")
public class AtmanDevicePlugin extends Plugin {
    private static final int MAX_REQUESTS = 12;
    private static final int CHUNK_BYTES = 16 * 1024;
    private static final String CLOSED = "The device response has closed";
    private static final String NOTICE_PREFIX = "/devices/notifications/";
    private static final String NOTICE_KEY = "pending";
    private final ExecutorService owner = Executors.newSingleThreadExecutor();
    private final ExecutorService readers = Executors.newFixedThreadPool(MAX_REQUESTS);
    private final Map<String, DeviceRequest> requests = new HashMap<>();
    private final OkHttpClient client = new OkHttpClient.Builder()
        .followRedirects(false)
        .followSslRedirects(false)
        .retryOnConnectionFailure(false)
        .connectTimeout(40, TimeUnit.SECONDS)
        .readTimeout(40, TimeUnit.SECONDS)
        .build();
    private DeviceVault vault;
    private SharedPreferences notices;
    private boolean scanning;

    @Override
    public void load() {
        vault = new DeviceVault(getContext());
        notices = getContext().getSharedPreferences("atman-notification", Context.MODE_PRIVATE);
    }

    @Override
    protected void handleOnNewIntent(Intent intent) {
        final String id;
        try {
            if (intent == null) return;
            id = intent.getStringExtra("notification_id");
            if (!isUuid(id) || UUID.fromString(id).version() != 4) return;
            intent.removeExtra("notification_id");
        } catch (RuntimeException ignored) { return; }
        dispatch(() -> {
            try {
                String record = vault.load();
                if (record == null) return;
                String device = new JSONObject(record).getJSONObject("credentials").getJSONObject("device").getString("id");
                if (!isUuid(device)) return;
                JSONObject pending = pendingNotice();
                if (pending != null && id.equals(pending.optString("notification_id")) &&
                    device.equals(pending.optString("device_id"))) return;
                String value = new JSONObject().put("notification_id", id).put("device_id", device)
                    .put("ticket", UUID.randomUUID().toString()).toString();
                if (notices.edit().putString(NOTICE_KEY, value).commit()) {
                    execute(() -> notifyListeners("notificationOpen", new JSObject()));
                }
            } catch (Exception ignored) { /* No authorized device is available for this click. */ }
        });
    }

    private JSONObject pendingNotice() throws Exception {
        String value = notices.getString(NOTICE_KEY, null);
        return value == null ? null : new JSONObject(value);
    }

    @PluginMethod
    public void pendingNotification(PluginCall call) {
        submit(call, () -> {
            try { call.resolve(value("notice", pendingNotice())); }
            catch (Exception error) { call.reject("Could not read the pending notification"); }
        });
    }

    @PluginMethod
    public void acknowledgeNotification(PluginCall call) {
        submit(call, () -> {
            try {
                String ticket = call.getString("ticket");
                if (!isUuid(ticket)) throw new IllegalArgumentException();
                JSONObject pending = pendingNotice();
                if (pending != null && ticket.equals(pending.getString("ticket")) &&
                    !notices.edit().remove(NOTICE_KEY).commit()) throw new IOException();
                call.resolve();
            } catch (Exception error) { call.reject("Could not acknowledge the notification"); }
        });
    }

    @PluginMethod
    public void scanInvitation(PluginCall call) {
        getActivity().runOnUiThread(() -> {
            if (scanning) {
                call.reject("The scanner is already open.");
            } else if (!getContext().getPackageManager().hasSystemFeature(PackageManager.FEATURE_CAMERA_ANY)) {
                call.reject("Camera scanning is unavailable. Paste the invitation link.");
            } else {
                ScanOptions options = new ScanOptions();
                options.setDesiredBarcodeFormats(ScanOptions.QR_CODE);
                options.setPrompt("Scan an Atman invitation");
                options.setBeepEnabled(false);
                options.setOrientationLocked(false);
                scanning = true;
                try {
                    startActivityForResult(call, options.createScanIntent(getContext()), "invitationScanned");
                } catch (ActivityNotFoundException | IllegalStateException error) {
                    scanning = false;
                    call.reject("Camera scanning is unavailable. Paste the invitation link.");
                }
            }
        });
    }

    @ActivityCallback
    private void invitationScanned(PluginCall call, ActivityResult result) {
        scanning = false;
        if (call == null) return;
        String link = ScanIntentResult.parseActivityResult(result.getResultCode(), result.getData()).getContents();
        if (link != null && size(link) > 2048) {
            call.reject("Scan an Atman invitation QR code.");
            return;
        }
        call.resolve(value("link", link));
    }

    @PluginMethod
    public void loadRecord(PluginCall call) {
        submit(call, () -> {
            try {
                call.resolve(value("record", vault.load()));
            } catch (Exception error) {
                call.reject("Secure device storage is unavailable");
            }
        });
    }

    @PluginMethod
    public void replaceRecord(PluginCall call) {
        submit(call, () -> {
            try {
                Object record = call.getData().opt("record");
                if (record instanceof String text) {
                    storedOrigin(deviceRecord(text));
                    vault.replace(text);
                } else if (record == JSONObject.NULL) {
                    vault.replace(null);
                } else {
                    throw new IllegalArgumentException();
                }
                call.resolve();
            } catch (Exception error) {
                call.reject("Could not save the secure device record");
            }
        });
    }

    @PluginMethod
    public void begin(PluginCall call) {
        beginRequest(call, false);
    }

    @PluginMethod
    public void beginRelay(PluginCall call) {
        beginRequest(call, true);
    }

    private void beginRequest(PluginCall call, boolean relay) {
        submit(call, () -> {
            try {
                String id = call.getString("id");
                if (id == null || !UUID.fromString(id).toString().equals(id) ||
                    requests.containsKey(id) || requests.size() >= MAX_REQUESTS) {
                    throw new IllegalArgumentException();
                }
                DeviceRequest request = new DeviceRequest(id, client.newCall(makeRequest(call, relay)), call);
                requests.put(id, request);
                request.transport.enqueue(new Callback() {
                    @Override
                    public void onFailure(Call transport, IOException error) {
                        dispatch(() -> retire(request, "The device request could not be opened"));
                    }

                    @Override
                    public void onResponse(Call transport, Response response) {
                        if (!dispatch(() -> install(request, response))) response.close();
                    }
                });
            } catch (Exception error) {
                call.reject("The device request could not be opened");
            }
        });
    }

    @PluginMethod
    public void read(PluginCall call) {
        submit(call, () -> {
            DeviceRequest request = requests.get(call.getString("id"));
            if (request == null || request.response == null || request.waiting != null) {
                call.reject(CLOSED);
                return;
            }
            request.waiting = call;
            Response response = request.response;
            readers.execute(() -> readChunk(request, response));
        });
    }

    @PluginMethod
    public void cancel(PluginCall call) {
        submit(call, () -> {
            DeviceRequest request = requests.get(call.getString("id"));
            if (request != null) retire(request, CLOSED);
            call.resolve();
        });
    }

    private void install(DeviceRequest request, Response response) {
        if (requests.get(request.id) != request) {
            response.close();
            return;
        }
        request.response = response;
        JSObject metadata = value("status", response.code());
        String contentType = response.header("Content-Type");
        if (contentType != null) metadata.put("contentType", contentType);
        PluginCall started = request.started;
        request.started = null;
        started.resolve(metadata);
    }

    private void readChunk(DeviceRequest request, Response response) {
        byte[] chunk = new byte[CHUNK_BYTES];
        try {
            int count = response.body() == null ? -1 : response.body().byteStream().read(chunk);
            String bytes = count < 0 ? null : Base64.encodeToString(chunk, 0, count, Base64.NO_WRAP);
            finishRead(request, response, bytes, false);
        } catch (IOException error) {
            finishRead(request, response, null, true);
        }
    }

    private void finishRead(DeviceRequest request, Response response, String bytes, boolean failed) {
        boolean dispatched = dispatch(() -> {
            if (requests.get(request.id) != request) {
                response.close();
                return;
            }
            PluginCall waiting = request.waiting;
            request.waiting = null;
            if (failed) waiting.reject(CLOSED);
            else waiting.resolve(value("bytes", bytes));
            if (bytes == null) retire(request, CLOSED);
        });
        if (!dispatched) response.close();
    }

    private void retire(DeviceRequest request, String message) {
        if (!requests.remove(request.id, request)) return;
        request.transport.cancel();
        if (request.response != null && request.waiting == null) request.response.close();
        if (request.started != null) request.started.reject(message);
        if (request.waiting != null) request.waiting.reject(message);
    }

    private void submit(PluginCall call, Runnable work) {
        if (!dispatch(work)) call.reject(CLOSED);
    }

    private boolean dispatch(Runnable work) {
        try {
            owner.execute(work);
            return true;
        } catch (RejectedExecutionException closed) {
            return false;
        }
    }

    @Override
    protected void handleOnDestroy() {
        dispatch(() -> {
            for (DeviceRequest request : requests.values().toArray(new DeviceRequest[0])) retire(request, CLOSED);
            readers.shutdown();
            client.dispatcher().executorService().shutdown();
            client.connectionPool().evictAll();
        });
        owner.shutdown();
    }

    private Request makeRequest(PluginCall call, boolean relay) throws Exception {
        String address = call.getString("url");
        String method = call.getString("method");
        if (address == null || size(address) > 2048) throw new IllegalArgumentException();
        URI target = new URI(address);
        HttpUrl url = HttpUrl.get(address);
        String origin = origin(target, url);
        String path = target.getRawPath();
        boolean pairing = !relay && path.equals("/devices/pair");
        boolean subscription = path.equals("/devices/notifications/subscription") ||
            path.equals("/devices/notifications/native/fcm");
        boolean notice = path.startsWith(NOTICE_PREFIX) && isUuid(path.substring(NOTICE_PREFIX.length()));
        boolean valid = switch (method) {
            case "GET" -> relay || subscription || path.equals("/events") ||
                path.equals("/devices/me") || path.equals("/devices/session") || notice;
            case "POST" -> relay || subscription || pairing || path.equals("/devices/refresh") || path.equals("/rpc");
            default -> false;
        };
        if (!valid || !path.equals(url.encodedPath()) ||
            ((relay || !path.equals("/events")) && target.getRawQuery() != null)) {
            throw new IllegalArgumentException();
        }
        String expectedAuthorization = null;
        if (!pairing) {
            JSONObject record = deviceRecord(vault.load());
            if (relay) expectedAuthorization = relayAuthorization(record, origin, path);
            else if (!origin.equals(storedOrigin(record))) throw new IllegalArgumentException();
            else if (subscription) expectedAuthorization = "Bearer " + record.getJSONObject("credentials").getString("access_token");
        }
        Request.Builder request = new Request.Builder().url(url);
        JSONObject headers = call.getObject("headers");
        if (headers == null) throw new IllegalArgumentException();
        boolean authorized = false;
        var names = headers.keys();
        while (names.hasNext()) {
            String key = names.next();
            String name = key.toLowerCase(Locale.ROOT);
            if (!(headers.get(key) instanceof String text) || size(text) > 1024 ||
                text.contains("\r") || text.contains("\n")) {
                throw new IllegalArgumentException();
            }
            switch (name) {
                case "authorization" -> {
                    if (pairing || authorized || !text.startsWith("Bearer ") ||
                        (expectedAuthorization != null && !text.equals(expectedAuthorization))) throw new IllegalArgumentException();
                    authorized = true;
                }
                case "accept", "content-type" -> {}
                default -> throw new IllegalArgumentException();
            }
            request.header(name, text);
        }
        if (!pairing && !authorized) throw new IllegalArgumentException();
        String body = call.getString("body");
        int limit = relay || subscription ? 4096 : 256 * 1024;
        if (body != null && (!method.equals("POST") || size(body) > limit)) {
            throw new IllegalArgumentException();
        }
        return request.method(method, method.equals("POST") ? RequestBody.create(body == null ? "" : body, null) : null).build();
    }

    private static JSONObject deviceRecord(String record) throws Exception {
        if (record == null || size(record) > 8192) throw new IllegalArgumentException();
        return new JSONObject(record);
    }

    private static String storedOrigin(JSONObject record) throws Exception {
        String value = record.getJSONObject("credentials").getString("origin");
        URI uri = new URI(value);
        if ((!uri.getRawPath().isEmpty() && !uri.getRawPath().equals("/")) || uri.getRawQuery() != null) {
            throw new IllegalArgumentException();
        }
        return origin(uri, HttpUrl.get(value));
    }

    private static String relayAuthorization(JSONObject record, String origin, String path) throws Exception {
        JSONObject binding = record.getJSONObject("notifications");
        String registration = binding.getString("registration_id");
        String key = binding.getString("owner_key");
        if (!binding.getString("kind").equals("relay") || !origin.equals(binding.getString("origin") + "/") ||
            !isUuid(registration) || !path.equals("/v1/registrations/" + registration) ||
            !key.matches("[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]")) throw new IllegalArgumentException();
        return "Bearer " + key;
    }

    private static String origin(URI raw, HttpUrl url) {
        if (!url.isHttps() || raw.getHost() == null || raw.getRawUserInfo() != null || raw.getRawFragment() != null) {
            throw new IllegalArgumentException();
        }
        return url.newBuilder().encodedPath("/").query(null).build().toString();
    }

    private static int size(String value) {
        return value.getBytes(StandardCharsets.UTF_8).length;
    }

    private static boolean isUuid(String value) {
        if (value == null || value.length() != 36) return false;
        try { return UUID.fromString(value).toString().equals(value); }
        catch (IllegalArgumentException ignored) { return false; }
    }

    private static JSObject value(String name, Object data) {
        return new JSObject().put(name, data == null ? JSONObject.NULL : data);
    }

    /** The owner serializes installation, pulls and retirement. Readers only hold the installed body. */
    private static final class DeviceRequest {
        final String id;
        final Call transport;
        PluginCall started;
        PluginCall waiting;
        Response response;

        DeviceRequest(String id, Call transport, PluginCall started) {
            this.id = id;
            this.transport = transport;
            this.started = started;
        }
    }
}
