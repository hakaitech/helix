;; End-to-end test of slices 1-3.
;;
;; - helix.status / helix.log come from slice 1
;; - helix.register-command + plugin: keymap come from slice 2
;; - helix.register-hook + doc/* + helix.current-mode come from slice 3

(helix.log "[init] running init.scm")

;; Slice 2: typable command bound to space-p
(helix.register-command "say-hi"
                        "Insert hello at cursor"
                        (lambda () (doc/insert "hello from steel!")))

;; Slice 3: bindings that query editor state
(helix.register-command "show-info"
                        "Report current doc state to status bar"
                        (lambda ()
                          (helix.status
                            (string-append "lines="
                                           (number->string (doc/line-count))
                                           " mode="
                                           (helix.current-mode)))))

;; Slice 3: event hooks
(helix.register-hook "OnModeSwitch"
                     (lambda ()
                       (helix.log (string-append "[hook OnModeSwitch] now in mode "
                                                 (helix.current-mode)))))

(helix.register-hook "PostCommand"
                     (lambda ()
                       (helix.log "[hook PostCommand] a command was executed")))
