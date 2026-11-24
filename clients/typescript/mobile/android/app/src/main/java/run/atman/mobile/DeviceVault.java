package run.atman.mobile;

import android.content.Context;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.AtomicFile;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.util.Arrays;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

final class DeviceVault {
    private static final String KEY_NAME = "run.atman.mobile.paired-device";
    private static final int MAX_RECORD_BYTES = 8192;
    private static final int NONCE_BYTES = 12;
    private static final int TAG_BYTES = 16;
    private static final byte VERSION = 1;
    private final AtomicFile file;

    DeviceVault(Context context) {
        file = new AtomicFile(new File(context.getNoBackupFilesDir(), "paired-device"));
    }

    String load() throws GeneralSecurityException, IOException {
        byte[] record;
        try {
            record = file.readFully();
        } catch (FileNotFoundException missing) {
            if (file.getBaseFile().exists()) throw missing;
            return null;
        }
        if (record.length == 0) return null;
        int overhead = 1 + NONCE_BYTES + TAG_BYTES;
        if (record.length <= overhead || record.length > MAX_RECORD_BYTES + overhead || record[0] != VERSION) {
            throw new IOException("Invalid secure device record");
        }
        Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
        cipher.init(Cipher.DECRYPT_MODE, key(false), new GCMParameterSpec(TAG_BYTES * 8, record, 1, NONCE_BYTES));
        byte[] plaintext = cipher.doFinal(record, 1 + NONCE_BYTES, record.length - 1 - NONCE_BYTES);
        return new String(plaintext, StandardCharsets.UTF_8);
    }

    void replace(String record) throws GeneralSecurityException, IOException {
        byte[] encoded = record == null ? new byte[0] : seal(record);
        FileOutputStream writer = file.startWrite();
        try {
            writer.write(encoded);
            writer.getFD().sync();
        } catch (IOException | RuntimeException error) {
            file.failWrite(writer);
            throw error;
        }
        file.finishWrite(writer);
        if (!Arrays.equals(encoded, file.readFully())) throw new IOException("Could not install the secure device record");
    }

    private byte[] seal(String record) throws GeneralSecurityException, IOException {
        byte[] bytes = record.getBytes(StandardCharsets.UTF_8);
        if (bytes.length == 0 || bytes.length > MAX_RECORD_BYTES) throw new IOException("Invalid secure device record");
        Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
        cipher.init(Cipher.ENCRYPT_MODE, key(true));
        byte[] nonce = cipher.getIV();
        if (nonce.length != NONCE_BYTES) throw new GeneralSecurityException("Invalid secure storage nonce");
        byte[] ciphertext = cipher.doFinal(bytes);
        return ByteBuffer.allocate(1 + NONCE_BYTES + ciphertext.length)
            .put(VERSION).put(nonce).put(ciphertext).array();
    }

    private SecretKey key(boolean create) throws GeneralSecurityException, IOException {
        KeyStore keys = KeyStore.getInstance("AndroidKeyStore");
        keys.load(null);
        if (keys.getKey(KEY_NAME, null) instanceof SecretKey existing) return existing;
        if (!create) throw new GeneralSecurityException("The secure device key is unavailable");
        KeyGenerator generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(KEY_NAME, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build());
        return generator.generateKey();
    }
}
