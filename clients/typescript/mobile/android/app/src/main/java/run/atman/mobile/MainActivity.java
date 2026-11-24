package run.atman.mobile;

import android.os.Bundle;
import androidx.core.splashscreen.SplashScreen;
import com.getcapacitor.BridgeActivity;

public class MainActivity extends BridgeActivity {
    @Override
    public void onCreate(Bundle state) {
        SplashScreen.installSplashScreen(this);
        registerPlugin(AtmanDevicePlugin.class);
        super.onCreate(state);
    }
}
