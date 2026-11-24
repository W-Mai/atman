package run.atman.mobile;

import com.google.firebase.messaging.FirebaseMessagingService;
import java.util.Set;
import java.util.concurrent.CopyOnWriteArraySet;

public class AtmanMessagingService extends FirebaseMessagingService {
    static final Set<Runnable> tokenObservers = new CopyOnWriteArraySet<>();

    @Override
    public void onNewToken(String token) {
        for (Runnable observer : tokenObservers) observer.run();
    }
}
